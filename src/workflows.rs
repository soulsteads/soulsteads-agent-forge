use crate::{
    core::{Core, WorkFault},
    dbos::admission::Envelope,
};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
fn fault(error: anyhow::Error) -> dbos::Error<WorkFault> {
    match error.downcast::<dbos::Error>() {
        Ok(e) => e.lift(),
        Err(e) => match e.downcast::<crate::dbos::admission::Error>() {
            Ok(crate::dbos::admission::Error::Sdk(e)) => e.lift(),
            _ => dbos::Error::Application(WorkFault),
        },
    }
}
// `dbos::Result<_, WorkFault>` is the DBOS SDK's workflow contract (its error type is
// fixed by the SDK and cannot be boxed here), so the lint is allowed on this function only.
#[allow(clippy::result_large_err)]
pub async fn execute(core: Arc<Core>, envelope: Envelope) -> dbos::Result<Value, WorkFault> {
    let selected = Arc::clone(&core);
    let input = envelope.clone();
    let definition = dbos::step("forge.admit", move || {
        let core = selected.clone();
        let e = input.clone();
        async move { core.current(&e).map_err(fault) }
    })
    .await?;
    core.current(&envelope).map_err(fault)?;
    if crate::flow_core::is_ledgered(&definition) {
        return crate::flow_runtime::execute(core, envelope, definition).await;
    }
    if definition["type"] == "echo"
        && let Some(delay) = definition["input"]["delay"].as_f64()
        && delay > 0.0
    {
        dbos::sleep::<WorkFault>(Duration::from_secs_f64(delay)).await?;
    }
    let selected = Arc::clone(&core);
    let input = envelope.clone();
    let result = dbos::step("forge.action", move || {
        let core = selected.clone();
        let e = input.clone();
        let d = definition.clone();
        async move {
            core.current(&e).map_err(fault)?;
            let output = match d["type"].as_str().unwrap_or("") {
                "echo" => serde_json::json!({"text":d["input"]["text"]}),
                "fail" => return Err(dbos::Error::Application(WorkFault)),
                _ => core.execute_effect(&e, &d).await.map_err(fault)?,
            };
            if let Some(expect) = d["input"]["expect"].as_str()
                && !output["text"].as_str().unwrap_or("").contains(expect)
            {
                return Err(dbos::Error::Application(WorkFault));
            }
            Ok(output)
        }
    })
    .await?;
    core.current(&envelope).map_err(fault)?;
    Ok(result)
}
// `dbos::Result<_, WorkFault>` is the DBOS SDK's workflow contract (its error type is
// fixed by the SDK and cannot be boxed here), so the lint is allowed on this function only.
#[allow(clippy::result_large_err)]
pub async fn scheduled(
    core: Arc<Core>,
    input: dbos::ScheduledWorkflowInput<crate::core::ScheduleContext>,
) -> dbos::Result<Value, WorkFault> {
    if input.context.home_id != core.policy.home_id {
        return Err(dbos::Error::Application(WorkFault));
    }
    // This is only an idempotent admission handoff. No child is awaited while
    // occupying a work slot; DBOS alone dequeues and executes the admitted run.
    // The native firing's own id (deterministic per due tick; clock-derived and
    // fixed at enqueue for a manual trigger) selects the admission key.
    let occurrence = dbos::workflow_id();
    dbos::step("forge.schedule_admission", move || {
        let core = core.clone();
        let context = input.context.clone();
        let due = input.scheduled_time;
        let occurrence = occurrence.clone();
        async move {
            core.schedule_handoff_for(&context, due, occurrence.as_deref())
                .await
                .map_err(fault)
        }
    })
    .await
}

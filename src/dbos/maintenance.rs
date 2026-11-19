//! The single native scheduled metadata reconciliation owner. It executes no leaves.
use super::admission::{AdmissionStore, BridgeProgress, Envelope, SubmissionBridge};
use super::{Error, Result, Runtime, RuntimeConfig};
use ::dbos::sysdb::types::{ScheduleStatus, WorkflowRecord};
use ::dbos::{Client, ScheduleSpec, ScheduledWorkflowInput};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
pub(crate) const NAME: &str = "forge_v1_submission_maintenance";
const CRON: &str = "*/10 * * * * *";
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Context {
    format_version: u8,
    home_id: String,
}
#[derive(Debug, thiserror::Error, Serialize, Deserialize)]
#[error("submission metadata reconciliation refused; inspect local bridge health")]
struct BridgeFault;
pub(super) struct InstalledBridge {
    pub bridge: Arc<SubmissionBridge>,
    spec: ScheduleSpec,
    context: Context,
}
pub(super) fn install(runtime: &Runtime, store: Arc<AdmissionStore>) -> Result<InstalledBridge> {
    let allowed = runtime
        .registrations
        .lock()
        .map_err(|_| Error::Config("registration set poisoned"))?
        .clone();
    if allowed.contains(NAME) {
        return Err(Error::Config("maintenance function name is reserved"));
    }
    let bridge = Arc::new(SubmissionBridge::new(
        store,
        runtime.config.clone(),
        allowed,
    )?);
    let context = Context {
        format_version: 1,
        home_id: runtime.config.home_id.clone(),
    };
    let expected = context.clone();
    let selected = Arc::clone(&bridge);
    let workflow =
        runtime.register_workflow(NAME, move |input: ScheduledWorkflowInput<Context>| {
            let bridge = Arc::clone(&selected);
            let expected = expected.clone();
            async move {
                if input.context != expected {
                    return Err(BridgeFault.into());
                }
                ::dbos::step("reconcile-admitted-submissions", move || {
                    let bridge = Arc::clone(&bridge);
                    async move {
                        match bridge.reconcile(32).await {
                            Ok(progress) => {
                                Ok::<BridgeProgress, ::dbos::Error<BridgeFault>>(progress)
                            }
                            Err(super::admission::Error::Sdk(error)) => Err(error.lift()),
                            Err(_) => Err(BridgeFault.into()),
                        }
                    }
                })
                .await
            }
        })?;
    let mut spec = ScheduleSpec::new(
        format!("forge-v1-{}-submission-maintenance", runtime.config.home_id),
        &workflow,
        CRON,
        &context,
    )?;
    spec.automatic_backfill = false;
    spec.queue_name = None;
    spec.app_name = Some(runtime.config.application.clone());
    Ok(InstalledBridge {
        bridge,
        spec,
        context,
    })
}
impl InstalledBridge {
    pub async fn prepare(
        &self,
        client: &Client,
        config: &RuntimeConfig,
        application: Option<&crate::core::Core>,
    ) -> Result<()> {
        let schedules = client.list_schedules(&Default::default()).await?;
        for schedule in &schedules {
            if schedule.schedule_name() != self.spec.schedule_name {
                application
                    .ok_or(Error::Config("application schedule validator missing"))?
                    .validate_schedule(schedule)
                    .map_err(|_| Error::Config("unauthorized or incompatible user schedule"))?;
                continue;
            }
            if schedule.schedule_name() != self.spec.schedule_name
                || schedule.workflow() != &self.spec.workflow
                || schedule.schedule() != CRON
                || schedule.status() != ScheduleStatus::Active
                || schedule.automatic_backfill()
                || schedule.queue_name().is_some()
                || schedule.cron_timezone().is_some()
                || schedule.app_name() != Some(config.application.as_str())
                || schedule.decode_context::<Context>()? != self.context
            {
                return Err(Error::Config(
                    "only the exact active fixed metadata maintenance schedule is admitted",
                ));
            }
        }
        if !schedules
            .iter()
            .any(|s| s.schedule_name() == self.spec.schedule_name)
        {
            client
                .apply_schedules(std::slice::from_ref(&self.spec))
                .await?;
        }
        self.bridge.reconcile(32).await?;
        Ok(())
    }
    pub async fn verify_pending(&self, row: &WorkflowRecord) -> Result<()> {
        let name = row.qualified_name().map(|v| v.to_string());
        if name.as_deref() == Some(NAME) {
            if row
                .queue_name
                .as_deref()
                .is_some_and(|name| name != ::dbos::sysdb::INTERNAL_QUEUE)
                || row.queue_partition_key.is_some()
            {
                return Err(Error::Config(
                    "maintenance may use only the internal metadata lane",
                ));
            }
            return Ok(());
        }
        let raw = row
            .input
            .as_deref()
            .ok_or(Error::Config("pending invocation input missing"))?;
        let envelope: Envelope = serde_json::from_str(raw).map_err(|_| {
            Error::Config("pending invocation input is not a closed Forge envelope")
        })?;
        if row.workflow_id != envelope.dbos_id {
            return Err(Error::Config(
                "pending native ID differs from canonical envelope ID",
            ));
        }
        self.bridge.store.verify(&envelope)?;
        // This read also validates SDK serialization, destination and all identity fields.
        self.bridge.verify_existing(&envelope).await?;
        Ok(())
    }
}

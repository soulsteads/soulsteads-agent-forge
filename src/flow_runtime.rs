//! Bounded orchestration in a native DBOS workflow body, never a nested step engine.
use crate::{
    core::{Core, Invalid, WorkFault},
    dbos::admission::Envelope,
    flow_schema,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, sync::Arc};

type FlowFuture<'a> = Pin<Box<dyn Future<Output = dbos::Result<Value, WorkFault>> + Send + 'a>>;
pub(crate) fn fault(_: impl std::fmt::Display) -> dbos::Error<WorkFault> {
    dbos::Error::Application(WorkFault)
}
pub(crate) fn reject(core: &Core, e: &Envelope, reason: &str) -> dbos::Error<WorkFault> {
    let _ = core.record("runtime", "flow_refused", &e.dbos_id, reason);
    fault(reason)
}
fn bounded(value: &Value) -> Result<()> {
    if serde_json::to_vec(value)?.len() > 1024 * 1024 {
        return Err(Invalid("flow context exceeds 1 MiB budget").into());
    }
    Ok(())
}
struct Budget {
    nodes: usize,
    calls: usize,
}
// `dbos::Result<_, WorkFault>` is the DBOS SDK's workflow contract (its error type is
// fixed by the SDK and cannot be boxed here), so the lint is allowed on this function only.
#[allow(clippy::result_large_err)]
pub(crate) async fn execute(
    core: Arc<Core>,
    e: Envelope,
    definition: Value,
) -> dbos::Result<Value, WorkFault> {
    core.flow_current(&e).map_err(fault)?;
    let input = core.invocation_params(&e).map_err(fault)?;
    let mut budget = Budget { nodes: 0, calls: 0 };
    evaluate(core, e, definition, input, "root".into(), 0, &mut budget).await
}
fn evaluate<'a>(
    core: Arc<Core>,
    e: Envelope,
    definition: Value,
    input: Value,
    path: String,
    depth: usize,
    budget: &'a mut Budget,
) -> FlowFuture<'a> {
    Box::pin(async move {
        budget.nodes += 1;
        if depth > 4 || budget.nodes + budget.calls > 64 {
            return Err(reject(
                &core,
                &e,
                "flow aggregate step/depth budget exceeded",
            ));
        }
        core.flow_current(&e).map_err(fault)?;
        core.validate_definition(e.invocation.principal(), &definition, "execute")
            .map_err(|error| {
                let reason = error
                    .downcast_ref::<Invalid>()
                    .map(|v| v.0.to_owned())
                    .or_else(|| {
                        error
                            .downcast_ref::<crate::core::Denied>()
                            .map(|v| v.0.to_owned())
                    })
                    .or_else(|| {
                        error
                            .downcast_ref::<crate::core::InvalidFlow>()
                            .map(ToString::to_string)
                    })
                    .unwrap_or_else(|| "resolved definition refused before dispatch".into());
                reject(&core, &e, &format!("{path}: {reason}"))
            })?;
        let params = core.merge_params(&definition, &input).map_err(fault)?;
        let mut context = json!({"params":params,"steps":{}});
        bounded(&context).map_err(fault)?;
        match definition["type"].as_str().unwrap_or("") {
            "workflow" => {
                let mut last = Value::Null;
                for step in definition["input"]["steps"]
                    .as_array()
                    .ok_or_else(|| reject(&core, &e, "workflow steps missing"))?
                {
                    core.flow_current(&e).map_err(fault)?;
                    let id = step["id"]
                        .as_str()
                        .ok_or_else(|| reject(&core, &e, "step id missing"))?;
                    let action = &step["action"];
                    let at = format!("{path}/step:{id}");
                    let refuse = |what: &str, error: anyhow::Error| {
                        reject(&core, &e, &format!("{at}: {what}: {error:#}"))
                    };
                    let (child, child_params) = if let Some(work_id) = action["work_id"].as_str() {
                        let child = core
                            .admitted(e.invocation.principal(), work_id)
                            .map_err(|error| refuse("referenced work unavailable", error))?;
                        let params = flow_schema::render(
                            action.get("params").unwrap_or(&json!({})),
                            &context,
                        )
                        .map_err(|error| refuse("parameter reference failed", error))?;
                        let child = if crate::flow_core::is_flow(&child) {
                            child
                        } else {
                            json!({"type":child["type"],"input":flow_schema::render_input(child["type"].as_str().unwrap_or(""),&child["input"],&json!({"params":params,"steps":{}})).map_err(|error| refuse("input reference failed", error))?})
                        };
                        (child, params)
                    } else if crate::flow_core::is_flow(action) {
                        let mut child = action.clone();
                        let params = flow_schema::render(
                            action["input"].get("params").unwrap_or(&json!({})),
                            &context,
                        )
                        .map_err(|error| refuse("parameter reference failed", error))?;
                        // Child defaults may refer to parent outputs only at this explicit boundary.
                        child["input"]["params"] = params.clone();
                        (child, params)
                    } else {
                        (
                            json!({"type":action["type"],"input":flow_schema::render_input(action["type"].as_str().unwrap_or(""),&action["input"],&context).map_err(|error| refuse("input reference failed", error))?}),
                            json!({}),
                        )
                    };
                    last = evaluate(
                        core.clone(),
                        e.clone(),
                        child,
                        child_params,
                        format!("{path}/step:{id}"),
                        depth + 1,
                        budget,
                    )
                    .await?;
                    context["steps"][id] = json!({"output":last});
                    bounded(&context).map_err(fault)?;
                }
                let output = if let Some(mapping) = definition["input"].get("output") {
                    flow_schema::render(mapping, &context).map_err(|error| {
                        reject(
                            &core,
                            &e,
                            &format!("{path}: output mapping failed: {error:#}"),
                        )
                    })?
                } else {
                    last
                };
                core.flow_current(&e).map_err(fault)?;
                Ok(json!({"steps":context["steps"],"output":output}))
            }
            "agent" => agent(core, e, definition, context, path, budget).await,
            // Nested harness steps were rendered once at their step boundary; only a root run renders here.
            "harness" if depth == 0 => {
                let input = flow_schema::render_input("harness", &definition["input"], &context)
                    .map_err(fault)?;
                leaf(core, e, json!({"type":"harness","input":input}), path).await
            }
            _ => leaf(core, e, definition, path).await,
        }
    })
}
// `dbos::Result<_, WorkFault>` is the DBOS SDK's workflow contract (its error type is
// fixed by the SDK and cannot be boxed here), so the lint is allowed on this function only.
#[allow(clippy::result_large_err)]
async fn leaf(
    core: Arc<Core>,
    e: Envelope,
    action: Value,
    slot: String,
) -> dbos::Result<Value, WorkFault> {
    core.flow_current(&e).map_err(fault)?;
    core.validate_leaf(e.invocation.principal(), &action, "execute")
        .map_err(|_| {
            reject(
                &core,
                &e,
                &format!("{slot}: resolved leaf schema or current authority refused"),
            )
        })?;
    if action["type"] == "approval" {
        return approval_gate(core, e, action, slot).await;
    }
    if action["type"] == "echo"
        && let Some(delay) = action["input"]["delay"].as_f64()
        && delay > 0.0
    {
        dbos::sleep::<WorkFault>(std::time::Duration::from_secs_f64(delay)).await?;
    }
    let selected = core.clone();
    let envelope = e.clone();
    let definition = action.clone();
    let effect_id = slot.clone();
    let name = format!("forge.flow.{slot}");
    let result = dbos::step(&name, move || {
        let core = selected.clone();
        let e = envelope.clone();
        let action = definition.clone();
        let slot = effect_id.clone();
        async move {
            core.flow_current(&e).map_err(fault)?;
            core.validate_leaf(e.invocation.principal(), &action, "execute")
                .map_err(|_| {
                    reject(
                        &core,
                        &e,
                        &format!("{slot}: resolved leaf schema or current authority refused"),
                    )
                })?;
            let result = match action["type"].as_str().unwrap_or("") {
                "echo" => json!({"text":action["input"]["text"]}),
                "fail" => return Err(reject(&core, &e, &format!("{slot}: explicit flow failure"))),
                _ => core
                    .flow_effect(&e, &slot, &action)
                    .await
                    .map_err(|error| {
                        let reason = if action["type"] == crate::workflow_credentials::KIND {
                            error.downcast_ref::<Invalid>().map(|v| v.0)
                        } else {
                            None
                        }
                        .unwrap_or(
                            "external call failed or is unresolved; inspect exact effect receipt",
                        );
                        reject(&core, &e, &format!("{slot}: {reason}"))
                    })?,
            };
            if let Some(expect) = action["input"]["expect"].as_str()
                && !result["text"].as_str().unwrap_or("").contains(expect)
            {
                return Err(reject(
                    &core,
                    &e,
                    &format!("{slot}: flow output assertion failed"),
                ));
            }
            Ok(result)
        }
    })
    .await?;
    core.flow_current(&e).map_err(fault)?;
    core.validate_leaf(e.invocation.principal(), &action, "execute")
        .map_err(|_| {
            reject(
                &core,
                &e,
                &format!("{slot}: resolved leaf schema or current authority refused"),
            )
        })?;
    if !matches!(action["type"].as_str(), Some("echo" | "fail")) {
        core.flow_verify_result(&e, &slot, &action, &result)
            .map_err(fault)?;
    }
    bounded(&result).map_err(fault)?;
    Ok(result)
}
// `dbos::Result<_, WorkFault>` is the DBOS SDK's workflow contract (its error type is
// fixed by the SDK and cannot be boxed here), so the lint is allowed on this function only.
#[allow(clippy::result_large_err)]
async fn agent(
    core: Arc<Core>,
    e: Envelope,
    definition: Value,
    context: Value,
    path: String,
    budget: &mut Budget,
) -> dbos::Result<Value, WorkFault> {
    let input = &definition["input"];
    let instructions =
        flow_schema::render(input.get("instructions").unwrap_or(&json!("")), &context)
            .map_err(fault)?;
    let prompt = flow_schema::render(&input["prompt"], &context).map_err(fault)?;
    let mut messages = vec![
        json!({"role":"system","content":instructions}),
        json!({"role":"user","content":prompt}),
    ];
    let mut tool_schemas = Vec::new();
    let mut allowed = std::collections::BTreeMap::new();
    for tool in input["tools"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let name = tool["name"]
            .as_str()
            .ok_or_else(|| reject(&core, &e, "tool name missing"))?;
        let kind = tool["type"]
            .as_str()
            .ok_or_else(|| reject(&core, &e, "tool type missing"))?;
        let plugin = core
            .plugin(kind)
            .ok_or_else(|| reject(&core, &e, "tool plugin unavailable"))?;
        allowed.insert(name.to_owned(), kind.to_owned());
        tool_schemas.push(json!({"type":"function","function":{"name":name,"description":tool.get("description").unwrap_or(&json!(plugin.description)),"parameters":plugin.schema}}));
    }
    let rounds = input["max_rounds"]
        .as_u64()
        .unwrap_or(flow_schema::MAX_ROUNDS as u64);
    let call_limit = input["max_tool_calls"]
        .as_u64()
        .unwrap_or(flow_schema::MAX_TOOL_CALLS as u64) as usize;
    let mut tools_used = Vec::new();
    let mut ids = std::collections::BTreeSet::new();
    for round in 0..rounds {
        core.flow_current(&e).map_err(fault)?;
        budget.calls += 1;
        if budget.nodes + budget.calls > 64 {
            return Err(reject(&core, &e, "aggregate flow action budget exceeded"));
        }
        bounded(&json!(messages)).map_err(fault)?;
        let mut request = json!({"messages":messages,"max_tokens":1024,"temperature":0,"chat_template_kwargs":{"enable_thinking":false}});
        if !tool_schemas.is_empty() {
            request["tools"] = json!(tool_schemas);
            request["tool_choice"] = json!("auto");
        }
        bounded(&request).map_err(fault)?;
        let action =
            json!({"type":"agent_turn","input":{"model":input["model"],"request":request}});
        let reply = leaf(
            core.clone(),
            e.clone(),
            action,
            format!("{path}/round:{round}/model"),
        )
        .await?;
        let message = &reply["message"];
        if message.get("role").is_some_and(|role| role != "assistant") {
            return Err(reject(&core, &e, "agent provider returned invalid role"));
        }
        let calls = message.get("tool_calls").filter(|v| !v.is_null());
        if calls.is_none_or(|calls| calls.as_array().is_some_and(Vec::is_empty)) {
            let text = message["content"]
                .as_str()
                .ok_or_else(|| reject(&core, &e, "agent final text missing"))?;
            let parsed = serde_json::from_str::<Value>(text).ok();
            if let Some(schema) = input.get("output_schema")
                && !parsed
                    .as_ref()
                    .is_some_and(|v| jsonschema::is_valid(schema, v))
            {
                return Err(reject(&core, &e, "agent output schema failed"));
            }
            messages.push(json!({"role":"assistant","content":text}));
            let output = json!({"text":text,"json":parsed,"model":reply["model"],"rounds":round+1,"tool_calls":tools_used,"messages":messages});
            bounded(&output).map_err(fault)?;
            core.flow_current(&e).map_err(fault)?;
            return Ok(output);
        }
        let calls = calls
            .and_then(Value::as_array)
            .ok_or_else(|| reject(&core, &e, "invalid agent tool_calls"))?;
        if round + 1 >= rounds
            || tools_used.len() + calls.len() > call_limit
            || budget.nodes + budget.calls + calls.len() > 64
        {
            return Err(reject(
                &core,
                &e,
                "agent round/tool budget exhausted before dispatch",
            ));
        }
        let mut validated = Vec::new();
        for call in calls {
            let id = call["id"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control))
                .ok_or_else(|| reject(&core, &e, "tool call id missing"))?;
            if !ids.insert(id.to_owned()) || call["type"] != "function" {
                return Err(reject(&core, &e, "duplicate or invalid tool call identity"));
            }
            let name = call["function"]["name"]
                .as_str()
                .ok_or_else(|| reject(&core, &e, "tool name missing"))?;
            let kind = allowed
                .get(name)
                .ok_or_else(|| reject(&core, &e, "agent selected ungranted tool"))?;
            let arguments = call["function"]["arguments"]
                .as_str()
                .filter(|s| s.len() <= 65536)
                .ok_or_else(|| reject(&core, &e, "tool arguments must be bounded JSON"))?;
            let arguments: Value = serde_json::from_str(arguments)
                .map_err(|_| reject(&core, &e, "tool arguments are malformed JSON"))?;
            let action = json!({"type":kind,"input":arguments});
            core.validate_leaf(e.invocation.principal(), &action, "execute")
                .map_err(|_| {
                    reject(
                        &core,
                        &e,
                        "resolved leaf schema or current authority refused",
                    )
                })?;
            validated.push((id.to_owned(), name.to_owned(), action));
        }
        messages.push(json!({"role":"assistant","content":message["content"],"tool_calls":calls}));
        for (index, (id, name, action)) in validated.into_iter().enumerate() {
            budget.calls += 1;
            let output = leaf(
                core.clone(),
                e.clone(),
                action.clone(),
                format!("{path}/round:{round}/tool:{index}"),
            )
            .await?;
            messages.push(json!({"role":"tool","tool_call_id":id,"content":serde_json::to_string(&output).map_err(fault)?}));
            tools_used.push(json!({"id":id,"name":name,"input":action["input"],"output":output}));
        }
    }
    Err(reject(&core, &e, "agent round bound exhausted"))
}

/// A durable approval gate: record the pending request, wait on the native DBOS
/// mailbox (checkpointed, so a restart resumes the remaining wait), then decide from
/// the approval record. Rejection or timeout fails this step like any refused step.
// `dbos::Result<_, WorkFault>` is the DBOS SDK's workflow contract.
#[allow(clippy::result_large_err)]
async fn approval_gate(
    core: Arc<Core>,
    e: Envelope,
    action: Value,
    slot: String,
) -> dbos::Result<Value, WorkFault> {
    core.flow_current(&e).map_err(fault)?;
    let input = &action["input"];
    let id = core.approval_request(&e, &slot, input).map_err(fault)?;
    let wait = std::time::Duration::from_secs(
        input["timeout_seconds"]
            .as_u64()
            .unwrap_or(crate::approvals::MAX_TIMEOUT_SECONDS),
    );
    let topic = crate::approvals::topic(&id);
    let _woken = dbos::recv::<Value, WorkFault>(Some(&topic), wait).await?;
    let name = format!("forge.flow.{slot}");
    let result = dbos::step(&name, move || {
        let (core, e, slot, id) = (core.clone(), e.clone(), slot.clone(), id.clone());
        async move {
            core.flow_current(&e).map_err(fault)?;
            core.approval_finalize(&id)
                .map_err(|reason| reject(&core, &e, &format!("{slot}: {reason}")))
        }
    })
    .await?;
    Ok(result)
}

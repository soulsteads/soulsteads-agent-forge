//! Shared MCP tools for the stdio adapter and Forge HTTP service.
use crate::client::ApiClient;
use anyhow::Result;
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
    },
    service::RequestContext,
    transport::async_rw::AsyncRwTransport,
};
use serde_json::{Value, json};
use std::sync::Arc;

const MAX_FRAME: usize = 1024 * 1024;

enum Bridge {
    Stdio(ApiClient),
    Http(Arc<crate::core::Core>),
}

#[derive(Clone)]
pub(crate) struct Principal(pub String);

fn principal(context: &RequestContext<RoleServer>) -> Option<&str> {
    context
        .extensions
        .get::<axum::http::request::Parts>()?
        .extensions
        .get::<Principal>()
        .map(|principal| principal.0.as_str())
}

fn tools() -> Vec<Tool> {
    let text = json!({"type":"string"});
    let identifier = json!({"type":"string","minLength":1,"maxLength":80});
    let run = json!({"type":"string","minLength":1,"maxLength":256});
    let definitions = [
        (
            "snapshot",
            "Read canonical health, permitted types/models, work, schedules, runs and failures.",
            json!({}),
            vec![],
            true,
        ),
        (
            "runners",
            "Read-only operator-approved native harness runner profiles with observed versions, availability and the adapter capability matrix. Workflows select a profile by name only.",
            json!({}),
            vec![],
            true,
        ),
        (
            "work",
            "Read one exact immutable scoped definition; snapshot work entries are summaries.",
            json!({"work_id":identifier}),
            vec!["work_id"],
            true,
        ),
        (
            "validate",
            "Check a work definition exactly as registration would, without storing it. Returns located, value-free diagnostics for invalid workflows.",
            json!({"definition":{"type":"object","properties":{"type":text,"input":{"type":"object"}},"required":["type","input"],"additionalProperties":false}}),
            vec!["definition"],
            true,
        ),
        (
            "runs",
            "Search scoped runs newest first by work_id and/or status with a bounded limit; page with the returned next_before cursor. Each entry includes status, failed workflow step and currently applicable actions.",
            json!({"work_id":identifier,"status":{"type":"string","enum":crate::run_views::STATUSES},"limit":{"type":"integer","minimum":1,"maximum":100},"before":{"type":"string","maxLength":32}}),
            vec![],
            true,
        ),
        (
            "register",
            "Validate and register immutable scoped work. Registration does not grant execution authority.",
            json!({"work_id":identifier,"definition":{"type":"object","properties":{"type":text,"input":{"type":"object"}},"required":["type","input"],"additionalProperties":false}}),
            vec!["work_id", "definition"],
            false,
        ),
        (
            "enable",
            "Enable or disable owned work. Disabling prevents subsequent dispatch.",
            json!({"work_id":identifier,"enabled":{"type":"boolean"}}),
            vec!["work_id", "enabled"],
            false,
        ),
        (
            "run",
            "Run permitted work; reuse the same key to repeat the same request safely. A new key requests a new operation; never use it to evade uncertain-effect reconciliation.",
            json!({"work_id":identifier,"key":identifier,"params":{"type":"object"}}),
            vec!["work_id", "key"],
            false,
        ),
        (
            "inspect",
            "Read canonical run state, steps, outputs/artifacts, effects and diagnostic evidence.",
            json!({"run_id":run,"effect_id":text,"step_id":{"type":"integer","minimum":0}}),
            vec!["run_id"],
            true,
        ),
        (
            "schedule",
            "Schedule permitted work with an explicit IANA time zone; its work is fixed once created (edit cron, zone, params, enabled and tags in place with forge_schedule_update; a different work needs a new name). Five fields use minutes first (normalized with leading second zero); six fields use seconds first. DBOS fires due ticks; missed ticks are not backfilled. Check timing first with forge_schedule_preview and verify actual due outcomes with forge_schedule_show.",
            json!({"name":identifier,"work_id":identifier,"cron":text,"tz":text,"params":{"type":"object"},"tags":{"type":"array","items":{"type":"string","maxLength":40},"maxItems":16}}),
            vec!["name", "work_id", "cron", "tz"],
            false,
        ),
        (
            "schedule_action",
            "Pause, resume or delete an owned schedule using its canonical actor-qualified name. Missed ticks are not backfilled on resume. Deleting keeps the retained definition and occurrence history readable.",
            json!({"name":text,"action":{"type":"string","enum":["pause","resume","delete"]}}),
            vec!["name", "action"],
            false,
        ),
        (
            "schedule_update",
            "Edit an owned schedule IN PLACE: cron, IANA zone, params (null clears), enabled and tags. Re-runs the creation checks, keeps every previous revision and the occurrence history, and never fires a tick twice or replays one that already fired (per-tick admission keys). The schedule's work is fixed; a different work needs a new schedule. An edit that changes nothing is a no-op. Requires the schedule capability.",
            json!({"name":text,"cron":text,"tz":text,"enabled":{"type":"boolean"},"params":{"type":["object","null"]},"tags":{"type":"array","items":{"type":"string","maxLength":40},"maxItems":16}}),
            vec!["name"],
            false,
        ),
        (
            "schedules",
            "List owned schedules, optionally only those carrying a tag, with status, cron, zone, next run, tags and revision.",
            json!({"tag":{"type":"string","maxLength":40}}),
            vec![],
            true,
        ),
        (
            "doctor",
            "Read-only diagnosis: config validity, settings, database and DBOS schema, runner probes, plugin release integrity, validation of every work definition and schedule, lost runs, orphan workflows, RECOVERY_REQUIRED runs, and missing capabilities/work types for newer operations (with the exact config.json snippet). Changes nothing.",
            json!({}),
            vec![],
            true,
        ),
        (
            "runner_test",
            "Probe one runner profile now (uncached): executable, sandbox launcher, version, and whether passed-through environment variables are set (names only). Runs the harness with --version only; no prompt, no model.",
            json!({"name":identifier}),
            vec!["name"],
            true,
        ),
        (
            "definitions_export",
            "Export your work and schedule definitions (not plugin-owned ones) as one importable JSON document.",
            json!({}),
            vec![],
            true,
        ),
        (
            "definitions_import",
            "Validate and import an exported definitions document with your authority. All-or-nothing validation; idempotent; an existing identity with a different definition is refused unless replace is true (work with an unsettled run is never replaced; a schedule's work is never changed). dry_run reports the plan without writing.",
            json!({"document":{"type":"object"},"replace":{"type":"boolean"},"dry_run":{"type":"boolean"}}),
            vec!["document"],
            false,
        ),
        (
            "service",
            "Manage the systemd --user unit for this home: status, preview (unit text), install, uninstall. The unit never contains the database credential (an EnvironmentFile reference or the private config.json supplies it); an existing unit is never overwritten. install/uninstall need the configure capability.",
            json!({"action":{"type":"string","enum":["status","preview","install","uninstall"]},"name":identifier,"env_file":{"type":"string","maxLength":4096},"start":{"type":"boolean"}}),
            vec!["action"],
            false,
        ),
        (
            "schedule_occurrences",
            "Read recent due occurrences of an owned schedule (newest first) with each firing's handoff status and its admitted run's own status, output and diagnostic; a failed handoff can still have admitted a run. Remains readable after pause, delete or restart.",
            json!({"name":text,"limit":{"type":"integer","minimum":1,"maximum":100}}),
            vec!["name"],
            true,
        ),
        (
            "schedule_show",
            "Read one owned schedule: retained immutable intent (work, params, cron, IANA zone), native DBOS status (ACTIVE, PAUSED or DELETED), last fired tick, upcoming ticks, catch-up policy (missed ticks are skipped), recent occurrences with each one's logical outcome (succeeded, failed, cancelled, running, not_admitted, handoff_pending) linked to its run, and applicable actions.",
            json!({"name":text,"limit":{"type":"integer","minimum":1,"maximum":100}}),
            vec!["name"],
            true,
        ),
        (
            "schedule_preview",
            "Validate a cron expression and IANA time zone exactly as schedule creation would and preview the next due ticks (local and UTC) without storing anything.",
            json!({"cron":text,"tz":text,"count":{"type":"integer","minimum":1,"maximum":20}}),
            vec!["cron", "tz"],
            true,
        ),
        (
            "schedule_trigger",
            "Run an owned saved schedule once, now, with its frozen work and params, through DBOS's native schedule trigger. Every call is a NEW manual run with a native trigger_id: there is no request-key idempotency, so never repeat a call to retry. If a call fails or times out, read forge_schedule_show for a manual occurrence first. It works on a PAUSED schedule, which stays paused; cron, params and last-fired tick are unchanged. Returns trigger_id, run_id when admitted, and the outcome. Read the result with forge_inspect.",
            json!({"name":text}),
            vec!["name"],
            false,
        ),
        (
            "queues",
            "Read the home's native DBOS queues: stored limits, home-local running/waiting counts (other actors' work is counted, never identified), what holds the slots, and your own waiting and running runs with why each waits (queue_capacity, native_delay, rate_limited or not_determined) from recorded evidence only. No queue position or start estimate is reported. Reads are bounded: members_listing and recent_window state whether your lists are complete (an incomplete list never proves absence; use forge_runs), and null capacity.free/saturated means unknown. The work queue also lists your recent results with measured wait and run time. Cancel a waiting run with forge_cancel; inspect with forge_inspect.",
            json!({"queue":{"type":"string","enum":["work","models","internal"]},"limit":{"type":"integer","minimum":1,"maximum":100}}),
            vec![],
            true,
        ),
        (
            "cancel",
            "Request cooperative cancellation. Cancellation cannot undo or prove termination of an active external effect.",
            json!({"run_id":run}),
            vec!["run_id"],
            false,
        ),
        (
            "retry",
            "Request a linked retry of definite failure with a stable key. Completed or uncertain effects cannot be replayed.",
            json!({"run_id":run,"key":identifier}),
            vec!["run_id", "key"],
            false,
        ),
        (
            "reconcile",
            "Record explicit operator evidence for a settled external operation. Requires reconciliation authority, terminal workflow, and settled=true. Never infer not_applied from timeout or cancellation.",
            json!({"run_id":run,"decision":{"type":"string","enum":["completed","not_applied"]},"note":{"type":"string","minLength":1,"maxLength":4096},"output":{},"settled":{"type":"boolean","const":true},"effect_id":text}),
            vec!["run_id", "decision", "note", "settled"],
            false,
        ),
        (
            "overview",
            "Read the operator overview: health (service, DBOS, database, runner and plugin availability), upcoming scheduled work, and recent failures (ERROR, RECOVERY_REQUIRED, uncertain effects) with their run IDs.",
            json!({}),
            vec![],
            true,
        ),
        (
            "agents",
            "List active and recent agent runs (harness/ACP steps) by runner profile and adapter with status, model and duration. Cancel one with forge_cancel; inspect with forge_inspect and forge_run_logs.",
            json!({"limit":{"type":"integer","minimum":1,"maximum":50}}),
            vec![],
            true,
        ),
        (
            "run_logs",
            "Read a run's logs and diagnostics, bounded and redacted. Without source: the index of sources (audit and one harness:<step> per harness step, with per-attempt diagnostics, tool calls and ACP permission decisions). With source: a page of entries after the cursor; continue with next_after until complete.",
            json!({"run_id":run,"source":text,"after":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":200}}),
            vec!["run_id"],
            true,
        ),
        (
            "schedule_stats",
            "Per-schedule run statistics: status, last fired, next due, and succeeded/failed/cancelled/running counts over the latest occurrences.",
            json!({}),
            vec![],
            true,
        ),
        (
            "approvals",
            "List your durable approval requests (workflow `approval` steps waiting for a decision, and decided ones), newest first. A pending one is actionable only while its run is still waiting.",
            json!({"state":{"type":"string","enum":["pending","approved","rejected","timed_out"]},"limit":{"type":"integer","minimum":1,"maximum":100}}),
            vec![],
            true,
        ),
        (
            "approve",
            "Approve one pending approval so its waiting run continues. Requires the approve capability and a decision note; a decision is final and recorded with your actor.",
            json!({"approval_id":text,"note":{"type":"string","minLength":1,"maxLength":1024}}),
            vec!["approval_id", "note"],
            false,
        ),
        (
            "reject",
            "Reject one pending approval; the waiting run's approval step fails with your note. Requires the approve capability and a decision note; final.",
            json!({"approval_id":text,"note":{"type":"string","minLength":1,"maxLength":1024}}),
            vec!["approval_id", "note"],
            false,
        ),
        (
            "skills",
            "List the skills compiled into this Forge build (read-only catalog).",
            json!({}),
            vec![],
            true,
        ),
        (
            "skill_show",
            "Read one compiled-in skill's full text.",
            json!({"name":text}),
            vec!["name"],
            true,
        ),
        (
            "settings",
            "Read the effective resolved settings with the source of each value (flag, env, config or default) and the value saved in config.json. Secrets are redacted.",
            json!({}),
            vec![],
            true,
        ),
        (
            "settings_set",
            "Save one config.json-backed setting after validation (null unsets it). Requires the configure capability and is audited. Takes effect only when the operator restarts the service; there is no restart operation.",
            json!({"key":text,"value":{}}),
            vec!["key", "value"],
            false,
        ),
        (
            "providers",
            "Read model aliases and harness runner profiles: saved configuration versus the running service, validation, availability and whether a restart is pending. Credentials appear only as environment variable names.",
            json!({}),
            vec![],
            true,
        ),
        (
            "provider_set",
            "Add or replace a model alias or runner profile in config.json using the same validation as profile load. Credentials are environment variable NAMES only. Requires the configure capability; audited; takes effect after an operator restart.",
            json!({"kind":{"type":"string","enum":["model","runner"]},"name":identifier,"definition":{"type":"object"}}),
            vec!["kind", "name", "definition"],
            false,
        ),
        (
            "provider_remove",
            "Remove a saved model alias or runner profile unless registered work still references it. Requires the configure capability; audited; takes effect after an operator restart.",
            json!({"kind":{"type":"string","enum":["model","runner"]},"name":identifier}),
            vec!["kind", "name"],
            false,
        ),
        (
            "plugin_installed",
            "List installed plugins with lifecycle state (installed, enabled, broken), contributions and health.",
            json!({}),
            vec![],
            true,
        ),
        (
            "plugin_install",
            "Install a plugin from an absolute local directory (stage, verify every digest, publish atomically). Installed DISABLED. With dry_run true: validate only and report the work types its jobs run, authority gaps and hints. Requires the plugins capability; denied attempts are audited.",
            json!({"source":{"type":"string","minLength":1,"maxLength":4096},"dry_run":{"type":"boolean"}}),
            vec!["source"],
            false,
        ),
        (
            "plugin_enable",
            "Enable an installed plugin: re-verify its release, describe it and materialize its jobs and workflows. Requires the plugins capability and your authority for each work type it runs (the error names a missing type).",
            json!({"id":identifier}),
            vec!["id"],
            false,
        ),
        (
            "plugin_disable",
            "Disable an enabled plugin (refused while it has active runs or calls). Requires the plugins capability.",
            json!({"id":identifier}),
            vec!["id"],
            false,
        ),
        (
            "plugin_uninstall",
            "Remove a disabled plugin that no work references. Requires the plugins capability.",
            json!({"id":identifier}),
            vec!["id"],
            false,
        ),
        (
            "subagent_spawn",
            "Spawn one subagent: admit a harness run on a named operator-approved runner profile (see forge_runners) with a prompt, optional instructions and an idempotency key. Thin wrapper over harness work (a fixed work definition per profile and permission ceiling); it grants no authority beyond your own register/execute rights and the profile's ceiling (read-only by default). The same key returns the same run; a new key is a new run. Poll with forge_subagent_poll; the default result is {\"result\": text} unless output_schema is given.",
            json!({"profile":identifier,"prompt":{"type":"string","minLength":1,"maxLength":65536},"instructions":{"type":"string","maxLength":65536},"key":identifier,"permissions":{"type":"string","enum":["read-only","workspace-write"]},"output_schema":{"type":"object"}}),
            vec!["profile", "prompt", "key"],
            false,
        ),
        (
            "subagent_poll",
            "Poll one subagent run: status, done flag, bounded output (max_bytes 256-65536, default 8192), diagnostic and applicable actions. Read-only; use forge_run_logs for the transcript, tool calls and permission decisions.",
            json!({"run_id":run,"max_bytes":{"type":"integer","minimum":256,"maximum":65536}}),
            vec!["run_id"],
            true,
        ),
        (
            "subagent_list",
            "List your active and recent subagent runs (those spawned with forge_subagent_spawn) by runner profile, adapter, model and duration.",
            json!({"limit":{"type":"integer","minimum":1,"maximum":50}}),
            vec![],
            true,
        ),
        (
            "subagent_cancel",
            "Request cooperative cancellation of one of your subagent runs (the existing cancel fence). Cancellation cannot undo or prove termination of an active external effect; an interrupted run may need reconciliation.",
            json!({"run_id":run}),
            vec!["run_id"],
            false,
        ),
        (
            "backups",
            "List backup archives in the configured backups directory, the state of the latest backup started through the service, and the exact CLI restore instruction (restore is CLI-only and needs the service stopped).",
            json!({}),
            vec![],
            true,
        ),
        (
            "backup_start",
            "Start one online backup of the running home and its database in the background (requires the transfer capability; one at a time). Poll forge_backups for its state.",
            json!({"name":identifier}),
            vec![],
            false,
        ),
    ];
    definitions.into_iter().map(|(name, description, properties, required, read_only)| {
        let schema = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
        Tool::new(format!("forge_{name}"), description.to_owned(), Arc::new(schema.as_object().expect("static schema object").clone()))
            .with_annotations(ToolAnnotations::new().read_only(read_only))
    }).collect()
}

/// Tools contributed by enabled installed plugins: stable `plugin_<id>__<tool>` names,
/// listed only for actors allowed to execute them. Fetched on every `tools/list`.
fn plugin_tools(descriptors: &Value) -> Vec<Tool> {
    descriptors
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| {
            let schema = d["input_schema"].as_object()?.clone();
            Some(
                Tool::new(
                    d["name"].as_str()?.to_owned(),
                    d["description"].as_str()?.to_owned(),
                    Arc::new(schema),
                )
                .with_annotations(ToolAnnotations::new().read_only(d["read_only"] == true)),
            )
        })
        .collect()
}

fn failure(message: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.to_owned())])
}

impl Bridge {
    async fn operation(
        &self,
        context: &RequestContext<RoleServer>,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, String> {
        match self {
            Self::Stdio(client) => client
                .invoke(method, params)
                .await
                .map_err(|error| error.to_string()),
            Self::Http(core) => {
                let actor =
                    principal(context).ok_or_else(|| "Forge bearer actor required".to_owned())?;
                crate::server::invoke_mcp(core, actor, method, params).await
            }
        }
    }
}

pub(crate) fn http(core: Arc<crate::core::Core>, host: std::net::IpAddr) -> axum::Router {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    };
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(["localhost".into(), host.to_string()])
        .enforce_origin_validation()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(crate::client::MAX_REQUEST);
    let service = StreamableHttpService::new(
        move || Ok(Bridge::Http(core.clone())),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    axum::Router::new().route_service("/mcp", service)
}

impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Use forge_snapshot to discover canonical state and supported types. All operations use the configured actor's authority. Follow the Forge operating skill. Unknown external effects require evidence-backed reconciliation, never blind replay. MCP cancellation is not evidence that an accepted operation stopped.")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        let mut all = tools();
        // Plugin tools are dynamic; an unreachable service just means none are listed.
        if let Ok(descriptors) = self.operation(&context, "plugin_tools", json!({})).await {
            all.extend(plugin_tools(&descriptors));
        }
        Ok(ListToolsResult {
            tools: all,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let arguments = Value::Object(request.arguments.clone().unwrap_or_default());
        let Some(tool) = tools().into_iter().find(|tool| tool.name == request.name) else {
            // Not a core tool: a plugin tool. The service re-checks that the plugin is
            // enabled, the actor may run it, and validates the arguments against its schema.
            if request.name.starts_with("plugin_") {
                return Ok(match self
                    .operation(
                        &context,
                        "plugin_mcp",
                        json!({"tool":request.name,"arguments":arguments}),
                    )
                    .await
                {
                    Ok(result) => {
                        let text = match &result["output"] {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        };
                        if result["is_error"] == true {
                            failure(&text)
                        } else {
                            CallToolResult::success(vec![ContentBlock::text(text)])
                        }
                    }
                    Err(error) => failure(&error),
                }
                .into());
            }
            return Err(ErrorData::invalid_params("Unknown Forge tool", None));
        };
        let params = arguments;
        let schema = Value::Object((*tool.input_schema).clone());
        let validator = jsonschema::validator_for(&schema)
            .map_err(|_| ErrorData::internal_error("Invalid Forge tool schema", None))?;
        if !validator.is_valid(&params) {
            return Ok(
                failure("Invalid tool arguments; inspect the published input schema.").into(),
            );
        }
        let method = request
            .name
            .strip_prefix("forge_")
            .expect("known tool prefix");
        let method = if method == "plugin_installed" {
            "plugin_list"
        } else {
            method
        };
        Ok(match self.operation(&context, method, params).await {
            Ok(result) => CallToolResult::success(vec![ContentBlock::text(result.to_string())]),
            Err(error) => failure(&error),
        }
        .into())
    }
}

/// Run the native MCP bridge without opening any Forge state or database.
pub async fn serve(url: String, token_env: String) -> Result<()> {
    let client = ApiClient::from_env(&url, &token_env)?;
    let (read, write) = rmcp::transport::stdio_pipes();
    let transport =
        AsyncRwTransport::<RoleServer, _, _>::new(read, write).with_max_line_length(MAX_FRAME);
    Bridge::Stdio(client)
        .serve(transport)
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_contracts_reject_authority_injection_and_describe_mutations() {
        let all = tools();
        assert_eq!(all.len(), 51);
        for tool in all {
            let schema = Value::Object((*tool.input_schema).clone());
            let validator = jsonschema::validator_for(&schema).unwrap();
            assert!(!validator.is_valid(&json!({"actor":"operator"})));
            let encoded = serde_json::to_value(&tool).unwrap();
            assert_eq!(
                encoded["annotations"]["readOnlyHint"],
                matches!(
                    tool.name.as_ref(),
                    "forge_snapshot"
                        | "forge_inspect"
                        | "forge_work"
                        | "forge_runners"
                        | "forge_schedule_occurrences"
                        | "forge_schedule_show"
                        | "forge_schedule_preview"
                        | "forge_validate"
                        | "forge_runs"
                        | "forge_queues"
                        | "forge_overview"
                        | "forge_agents"
                        | "forge_run_logs"
                        | "forge_schedule_stats"
                        | "forge_approvals"
                        | "forge_skills"
                        | "forge_skill_show"
                        | "forge_settings"
                        | "forge_providers"
                        | "forge_backups"
                        | "forge_plugin_installed"
                        | "forge_schedules"
                        | "forge_doctor"
                        | "forge_runner_test"
                        | "forge_definitions_export"
                        | "forge_subagent_poll"
                        | "forge_subagent_list"
                )
            );
        }
    }

    #[test]
    fn plugin_tools_become_namespaced_mcp_tools_and_malformed_descriptors_are_dropped() {
        let listed = plugin_tools(&json!([
            {"name":"plugin_demo__echo","description":"Echo","read_only":true,
             "input_schema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}},
            {"name":"plugin_bad__tool","description":"no schema"},
            {"description":"no name","input_schema":{"type":"object"}}
        ]));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "plugin_demo__echo");
        let encoded = serde_json::to_value(&listed[0]).unwrap();
        assert_eq!(encoded["annotations"]["readOnlyHint"], true);
        assert!(tools().iter().all(|t| !t.name.starts_with("plugin_")));
        assert!(plugin_tools(&json!("not an array")).is_empty());
    }
}

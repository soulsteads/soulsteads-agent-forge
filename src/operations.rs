//! Operational CLI: named commands over the canonical API only.
use crate::client::{ApiClient, ClientError, MAX_REQUEST};
use clap::{Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Subcommand)]
pub enum Work {
    /// List authority-scoped immutable work definitions.
    List,
    /// Read the complete immutable scoped definition.
    Show {
        id: String,
    },
    /// Register a definition JSON object from a file, or '-' for stdin.
    Register {
        id: String,
        #[arg(long)]
        definition: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    /// Reuse the same key after a lost response; a new key is a new operation.
    Run {
        id: String,
        #[arg(long)]
        key: String,
        /// Invocation params JSON file, or '-' for stdin.
        #[arg(long)]
        params: Option<String>,
    },
}
#[derive(Subcommand)]
pub enum Runs {
    /// List the most recent 100 authority-scoped runs (raw JSON projection).
    List,
    /// Find runs by workflow/work ID and status, newest first (human-readable; --json for the stable result).
    Search {
        /// Only runs of this work/workflow ID.
        #[arg(long = "work")]
        work: Option<String>,
        /// Only runs in this status (e.g. SUCCESS, ERROR, PENDING, CANCELLED, RECOVERY_REQUIRED).
        #[arg(long)]
        status: Option<String>,
        /// Maximum results, 1-100 (default 25).
        #[arg(long)]
        limit: Option<i64>,
        /// Page cursor printed by a previous search.
        #[arg(long)]
        before: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Explain one run: status, failure, per-step state/output and available actions.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Bounded, redacted logs and diagnostics. Without --source: the index of sources.
    /// With one (`audit` or `harness:STEP`): a page after --after.
    Logs {
        id: String,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        after: Option<i64>,
        #[arg(long)]
        limit: Option<i64>,
    },
    Inspect {
        id: String,
        #[arg(long, conflicts_with = "step_id")]
        effect_id: Option<String>,
        #[arg(long, conflicts_with = "effect_id")]
        step_id: Option<u32>,
    },
    Cancel {
        id: String,
    },
    Retry {
        id: String,
        #[arg(long)]
        key: String,
    },
    /// Record settled provider evidence; never infer no effect from timeout.
    Reconcile {
        id: String,
        #[arg(long, value_enum)]
        decision: Decision,
        #[arg(long)]
        note: String,
        /// Explicit acknowledgement that the external operation has settled.
        #[arg(long, required = true)]
        settled: bool,
        /// Provider output JSON file, or '-' for stdin.
        #[arg(long)]
        output: Option<String>,
        /// Exact unresolved flow effect from runs inspect.
        #[arg(long)]
        effect_id: Option<String>,
    },
}
#[derive(Clone, ValueEnum)]
pub enum Decision {
    Completed,
    NotApplied,
}
#[derive(Subcommand)]
pub enum Schedules {
    /// Raw JSON list of owned schedules (stable API projection).
    List {
        /// Only schedules carrying this tag.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Owned schedules as a table: status, cron, zone, last fired and next due tick.
    Status {
        #[arg(long)]
        json: bool,
        /// Only schedules carrying this tag.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Per-schedule statistics: last fired, next due, success/failure counts.
    Stats,
    /// One schedule: retained intent, native status, catch-up policy, upcoming ticks
    /// and recent occurrences with each run's outcome.
    Show {
        name: String,
        #[arg(long)]
        limit: Option<u8>,
        #[arg(long)]
        json: bool,
    },
    /// Recent due occurrences as a table: handoff status vs. the admitted run's outcome.
    History {
        name: String,
        #[arg(long)]
        limit: Option<u8>,
        #[arg(long)]
        json: bool,
    },
    /// Run a saved schedule once now with its frozen work and params (a NEW manual run,
    /// never retried; works on a paused schedule, which stays paused).
    Run {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Validate cron + IANA zone exactly as create would and preview due ticks; stores nothing.
    Check {
        #[arg(long)]
        cron: String,
        #[arg(long)]
        tz: String,
        #[arg(long)]
        count: Option<u8>,
        #[arg(long)]
        json: bool,
    },
    /// Create a schedule (prints the JSON list; a summary goes to stderr). Its work is fixed;
    /// change timing, params, enabled state and tags in place with `schedules edit`.
    Create {
        name: String,
        #[arg(long)]
        work_id: String,
        #[arg(long)]
        cron: String,
        #[arg(long)]
        tz: String,
        /// Scheduled invocation params JSON file, or '-' for stdin.
        #[arg(long)]
        params: Option<String>,
        /// Tag (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Edit a schedule in place: cron, time zone, params, enabled state, tags. Occurrence
    /// history is kept and no tick fires twice; omitted fields are unchanged.
    Edit {
        name: String,
        #[arg(long)]
        cron: Option<String>,
        #[arg(long)]
        tz: Option<String>,
        /// New params JSON file, or '-' for stdin.
        #[arg(long, conflicts_with = "clear_params")]
        params: Option<String>,
        /// Remove the scheduled params.
        #[arg(long)]
        clear_params: bool,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
        /// Replace the tag set (repeatable).
        #[arg(long = "tag", conflicts_with = "clear_tags")]
        tags: Vec<String>,
        /// Remove every tag.
        #[arg(long)]
        clear_tags: bool,
    },
    Pause {
        name: String,
    },
    Resume {
        name: String,
    },
    Delete {
        name: String,
    },
    /// Read recent due occurrences of a schedule and each admitted run's outcome.
    Occurrences {
        name: String,
        #[arg(long)]
        limit: Option<u8>,
    },
}
#[derive(Subcommand)]
pub enum Queues {
    /// Every native queue: stored limits, running/waiting counts, and your running and
    /// waiting work with why each waits (human-readable; --json for the stable result).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// One queue (work, models or internal): your members, why each waits, what holds
    /// the slots, and your recent results with measured wait and run time.
    Show {
        /// work, models or internal.
        queue: String,
        /// Most of your members to list, 1-100 (default 50).
        #[arg(long)]
        limit: Option<i64>,
        #[arg(long)]
        json: bool,
    },
}
#[derive(Subcommand)]
pub enum Plugins {
    /// List plugin work types (configured executables and enabled installed plugins) with descriptions and input schemas.
    List,
    /// Read a plugin type's cached description and input schema (`plugin.NAME`), or an installed plugin's manifest summary by id.
    Show {
        /// `plugin.NAME` work type, or an installed plugin id.
        r#type: String,
        #[arg(long)]
        json: bool,
        /// Re-verify an installed plugin's release digests and re-describe it.
        #[arg(long)]
        check: bool,
    },
    /// Check input with canonical registration policy and schema checks. Runs nothing.
    Validate {
        r#type: String,
        /// Plugin input JSON file, or '-' for stdin (not a complete work definition).
        #[arg(long)]
        input: String,
        #[arg(long)]
        json: bool,
    },
    /// Installed plugins with lifecycle state, contributions and health.
    Installed {
        #[arg(long)]
        json: bool,
    },
    /// Validate a plugin directory (manifest, digests, collisions) without installing it.
    Check { source: PathBuf },
    /// Install a plugin directory: stage, verify every digest, atomically publish. Installed DISABLED.
    Install { source: PathBuf },
    /// Enable an installed plugin: verify, describe, materialize its jobs and workflows.
    Enable { id: String },
    /// Disable a plugin (refused while it has active runs or calls).
    Disable { id: String },
    /// Remove a disabled plugin that no work references.
    Uninstall { id: String },
    /// Run a manifest-declared plugin CLI command through the service.
    Run {
        id: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum Runners {
    /// List operator-approved harness runner profiles, versions and the capability matrix.
    List,
    /// Probe one runner profile now (executable, sandbox, version, passed-through variables).
    /// Runs the harness with --version only: no prompt and no model.
    Test { name: String },
}

#[derive(Subcommand)]
pub enum Definitions {
    /// Export your work and schedule definitions as one JSON document.
    Export {
        /// Write to this file instead of stdout.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Validate and import an exported document (all-or-nothing, idempotent).
    Import {
        /// Document file, or '-' for stdin.
        file: String,
        /// Overwrite colliding identities: replaces work with no unsettled run and edits
        /// schedules in place. Without it a collision refuses the whole import.
        #[arg(long)]
        replace: bool,
        /// Report the plan and any problems without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub enum Service {
    /// The unit text that install would write (nothing is written).
    Preview {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        env_file: Option<PathBuf>,
    },
    /// Install a systemd --user unit for this home. The database credential is never put in
    /// the unit: --env-file names a private (0600) file that defines it.
    Install {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        env_file: Option<PathBuf>,
        /// Also enable and start it now.
        #[arg(long)]
        start: bool,
    },
    /// Stop, disable and remove a unit this tool generated for this home.
    Uninstall {
        #[arg(long)]
        name: Option<String>,
    },
    /// Whether the unit is installed, active and enabled.
    Status {
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum Approvals {
    /// Your approval requests (workflow `approval` steps), newest first.
    List {
        /// pending, approved, rejected or timed_out.
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Approve one pending approval; the waiting run continues. Final.
    Approve {
        id: String,
        #[arg(long)]
        note: String,
    },
    /// Reject one pending approval; the waiting run's step fails. Final.
    Reject {
        id: String,
        #[arg(long)]
        note: String,
    },
}
#[derive(Subcommand)]
pub enum Subagents {
    /// Spawn a subagent: one harness run on an operator-approved runner profile.
    Spawn {
        /// Runner profile name (see `runners list`).
        #[arg(long)]
        profile: String,
        /// Prompt text; or use --prompt-file.
        #[arg(
            long,
            conflicts_with = "prompt_file",
            required_unless_present = "prompt_file"
        )]
        prompt: Option<String>,
        /// Prompt text file, or '-' for stdin.
        #[arg(long)]
        prompt_file: Option<String>,
        #[arg(long)]
        instructions: Option<String>,
        /// Idempotency key: the same key repeats the same run; a new key is a new run.
        #[arg(long)]
        key: String,
        /// read-only (default) or workspace-write; capped by the profile.
        #[arg(long)]
        permissions: Option<String>,
        /// JSON Schema file the final answer must satisfy (default: {"result": text}).
        #[arg(long)]
        output_schema: Option<String>,
    },
    /// Status and bounded output of one subagent run.
    Poll {
        run_id: String,
        #[arg(long)]
        max_bytes: Option<i64>,
    },
    /// Active and recent subagent runs.
    List {
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Request cooperative cancellation of a subagent run.
    Cancel { run_id: String },
}
#[derive(Subcommand)]
pub enum Skills {
    /// Skills compiled into this build.
    List,
    /// Print one skill's text.
    Show { name: String },
}
#[derive(Subcommand)]
pub enum Settings {
    /// Effective resolved settings with the source of each value (secrets redacted).
    Show,
    /// Save a config.json-backed setting (VALUE is JSON, e.g. 3 or '"text"'). Takes effect after a restart.
    Set { key: String, value: String },
    /// Remove a saved setting so the default applies again.
    Unset { key: String },
}
#[derive(Subcommand)]
pub enum Providers {
    /// Saved versus running model aliases and runner profiles.
    List,
    /// Add or replace a model alias from a JSON file ('-' for stdin).
    SetModel {
        name: String,
        #[arg(long)]
        definition: String,
    },
    /// Add or replace a runner profile from a JSON file ('-' for stdin).
    SetRunner {
        name: String,
        #[arg(long)]
        definition: String,
    },
    /// Remove a saved model alias or runner profile not referenced by work.
    RemoveModel {
        name: String,
    },
    RemoveRunner {
        name: String,
    },
}
#[derive(Subcommand)]
pub enum Backups {
    /// Backup archives, the latest backup job, and the restore instruction.
    List,
    /// Start an online backup of the running service in the background.
    Start {
        #[arg(long)]
        name: Option<String>,
    },
}

pub enum Operation {
    Snapshot,
    Overview,
    Agents { limit: Option<i64> },
    Approvals(Approvals),
    Subagents(Subagents),
    Skills(Skills),
    Settings(Settings),
    Providers(Providers),
    Backups(Backups),
    Workflows(crate::cli_views::Workflows),
    Runners(Runners),
    Definitions(Definitions),
    Work(Work),
    Runs(Runs),
    Schedules(Schedules),
    Queues(Queues),
    Plugins(Plugins),
}
fn failure(message: &str) -> ClientError {
    ClientError(message.to_owned())
}
fn read_json(mut input: impl Read) -> Result<Value, ClientError> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take((MAX_REQUEST + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| failure("Cannot read JSON input"))?;
    if bytes.len() > MAX_REQUEST {
        return Err(failure("JSON input exceeds 1 MiB limit"));
    }
    serde_json::from_slice(&bytes).map_err(|_| failure("Input must contain one valid JSON value"))
}
/// A bounded UTF-8 text input from a file, or '-' for stdin.
pub(crate) fn read_text(path: &str) -> Result<String, ClientError> {
    let mut bytes = Vec::new();
    let reader: Box<dyn Read> = if path == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(
            std::fs::File::open(Path::new(path))
                .map_err(|_| failure("Cannot open text input file"))?,
        )
    };
    reader
        .take((MAX_REQUEST + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| failure("Cannot read text input"))?;
    if bytes.len() > MAX_REQUEST {
        return Err(failure("Text input exceeds 1 MiB limit"));
    }
    String::from_utf8(bytes).map_err(|_| failure("Text input must be UTF-8"))
}
pub(crate) fn json_input(path: &str) -> Result<Value, ClientError> {
    if path == "-" {
        read_json(std::io::stdin().lock())
    } else {
        let file = std::fs::File::open(Path::new(path))
            .map_err(|_| failure("Cannot open JSON input file"))?;
        read_json(file)
    }
}
fn with_optional(mut value: Value, key: &str, extra: Option<Value>) -> Value {
    if let Some(extra) = extra {
        value[key] = extra;
    }
    value
}
impl Operation {
    pub async fn execute(self, url: &str, token_env: &str) -> anyhow::Result<()> {
        let client = ApiClient::from_env(url, token_env)?;
        let (method, params, projection) = match self {
            Self::Plugins(command) => return crate::plugin_views::plugins(&client, command).await,
            Self::Workflows(command) => return crate::cli_views::workflows(&client, command).await,
            Self::Queues(command) => return crate::cli_views::queues(&client, command).await,
            Self::Runs(Runs::Search {
                work,
                status,
                limit,
                before,
                json,
            }) => {
                return crate::cli_views::run_search(&client, work, status, limit, before, json)
                    .await;
            }
            Self::Runs(Runs::Show { id, json }) => {
                return crate::cli_views::run_show(&client, id, json).await;
            }
            Self::Schedules(
                command @ (Schedules::Status { .. }
                | Schedules::Show { .. }
                | Schedules::History { .. }
                | Schedules::Check { .. }
                | Schedules::Run { .. }),
            ) => return crate::cli_views::schedules(&client, command).await,
            Self::Snapshot => ("snapshot", json!({}), None),
            Self::Overview => ("overview", json!({}), None),
            Self::Agents { limit } => (
                "agents",
                with_optional(json!({}), "limit", limit.map(|l| json!(l))),
                None,
            ),
            Self::Approvals(Approvals::List { state, limit }) => (
                "approvals",
                with_optional(
                    with_optional(json!({}), "state", state.map(Value::String)),
                    "limit",
                    limit.map(|l| json!(l)),
                ),
                None,
            ),
            Self::Approvals(Approvals::Approve { id, note }) => {
                ("approve", json!({"approval_id":id,"note":note}), None)
            }
            Self::Approvals(Approvals::Reject { id, note }) => {
                ("reject", json!({"approval_id":id,"note":note}), None)
            }
            Self::Subagents(Subagents::Spawn {
                profile,
                prompt,
                prompt_file,
                instructions,
                key,
                permissions,
                output_schema,
            }) => {
                let prompt = match (prompt, prompt_file) {
                    (Some(text), _) => text,
                    (None, Some(path)) => read_text(&path)?,
                    (None, None) => {
                        return Err(failure("--prompt or --prompt-file is required").into());
                    }
                };
                let mut params = json!({"profile":profile,"prompt":prompt,"key":key});
                if let Some(i) = instructions {
                    params["instructions"] = json!(i);
                }
                if let Some(p) = permissions {
                    params["permissions"] = json!(p);
                }
                if let Some(path) = output_schema {
                    params["output_schema"] = json_input(&path)?;
                }
                ("subagent_spawn", params, None)
            }
            Self::Subagents(Subagents::Poll { run_id, max_bytes }) => (
                "subagent_poll",
                with_optional(
                    json!({"run_id":run_id}),
                    "max_bytes",
                    max_bytes.map(|v| json!(v)),
                ),
                None,
            ),
            Self::Subagents(Subagents::List { limit }) => (
                "subagent_list",
                with_optional(json!({}), "limit", limit.map(|v| json!(v))),
                None,
            ),
            Self::Subagents(Subagents::Cancel { run_id }) => {
                ("subagent_cancel", json!({"run_id":run_id}), None)
            }
            Self::Skills(Skills::List) => ("skills", json!({}), None),
            Self::Skills(Skills::Show { name }) => ("skill_show", json!({"name":name}), None),
            Self::Settings(Settings::Show) => ("settings", json!({}), None),
            Self::Settings(Settings::Set { key, value }) => (
                "settings_set",
                json!({"key":key,"value":serde_json::from_str::<Value>(&value).map_err(|_| failure("VALUE must be valid JSON"))?}),
                None,
            ),
            Self::Settings(Settings::Unset { key }) => {
                ("settings_set", json!({"key":key,"value":null}), None)
            }
            Self::Providers(Providers::List) => ("providers", json!({}), None),
            Self::Providers(Providers::SetModel { name, definition }) => (
                "provider_set",
                json!({"kind":"model","name":name,"definition":json_input(&definition)?}),
                None,
            ),
            Self::Providers(Providers::SetRunner { name, definition }) => (
                "provider_set",
                json!({"kind":"runner","name":name,"definition":json_input(&definition)?}),
                None,
            ),
            Self::Providers(Providers::RemoveModel { name }) => {
                ("provider_remove", json!({"kind":"model","name":name}), None)
            }
            Self::Providers(Providers::RemoveRunner { name }) => (
                "provider_remove",
                json!({"kind":"runner","name":name}),
                None,
            ),
            Self::Backups(Backups::List) => ("backups", json!({}), None),
            Self::Backups(Backups::Start { name }) => (
                "backup_start",
                with_optional(json!({}), "name", name.map(Value::String)),
                None,
            ),
            Self::Runs(Runs::Logs {
                id,
                source,
                after,
                limit,
            }) => (
                "run_logs",
                with_optional(
                    with_optional(
                        with_optional(json!({"run_id":id}), "source", source.map(Value::String)),
                        "after",
                        after.map(|v| json!(v)),
                    ),
                    "limit",
                    limit.map(|v| json!(v)),
                ),
                None,
            ),
            Self::Schedules(Schedules::Stats) => ("schedule_stats", json!({}), None),
            Self::Runners(Runners::List) => ("runners", json!({}), None),
            Self::Runners(Runners::Test { name }) => ("runner_test", json!({"name":name}), None),
            Self::Definitions(Definitions::Export { output }) => {
                let doc = client.invoke("definitions_export", json!({})).await?;
                let text = serde_json::to_string_pretty(&doc)?;
                match output {
                    Some(path) => {
                        std::fs::write(&path, format!("{text}\n"))
                            .map_err(|_| failure("Cannot write the export file"))?;
                        eprintln!(
                            "Exported {} work definition(s) and {} schedule(s) to {}",
                            doc["work"].as_array().map_or(0, Vec::len),
                            doc["schedules"].as_array().map_or(0, Vec::len),
                            path.display()
                        );
                    }
                    None => println!("{text}"),
                }
                return Ok(());
            }
            Self::Definitions(Definitions::Import {
                file,
                replace,
                dry_run,
            }) => (
                "definitions_import",
                json!({"document":json_input(&file)?,"replace":replace,"dry_run":dry_run}),
                None,
            ),
            Self::Work(Work::Show { id }) => ("work", json!({"work_id":id}), None),
            Self::Work(Work::List) => ("snapshot", json!({}), Some("work")),
            Self::Runs(Runs::List) => ("snapshot", json!({}), Some("runs")),
            Self::Schedules(Schedules::List { tag }) => (
                "schedules",
                with_optional(json!({}), "tag", tag.map(Value::String)),
                None,
            ),
            Self::Work(Work::Register { id, definition }) => (
                "register",
                json!({"work_id":id,"definition":json_input(&definition)?}),
                None,
            ),
            Self::Work(Work::Enable { id }) => {
                ("enable", json!({"work_id":id,"enabled":true}), None)
            }
            Self::Work(Work::Disable { id }) => {
                ("enable", json!({"work_id":id,"enabled":false}), None)
            }
            Self::Work(Work::Run { id, key, params }) => (
                "run",
                with_optional(
                    json!({"work_id":id,"key":key}),
                    "params",
                    params.as_deref().map(json_input).transpose()?,
                ),
                None,
            ),
            Self::Runs(Runs::Inspect {
                id,
                effect_id,
                step_id,
            }) => (
                "inspect",
                with_optional(
                    with_optional(
                        json!({"run_id":id}),
                        "effect_id",
                        effect_id.map(Value::String),
                    ),
                    "step_id",
                    step_id.map(|id| json!(id)),
                ),
                None,
            ),
            Self::Runs(Runs::Cancel { id }) => ("cancel", json!({"run_id":id}), None),
            Self::Runs(Runs::Retry { id, key }) => ("retry", json!({"run_id":id,"key":key}), None),
            Self::Runs(Runs::Reconcile {
                id,
                decision,
                note,
                settled,
                output,
                effect_id,
            }) => (
                "reconcile",
                with_optional(
                    json!({"run_id":id,"decision": match decision {Decision::Completed=>"completed",Decision::NotApplied=>"not_applied"},"note":note,"settled":settled,"output": output.as_deref().map(json_input).transpose()?.unwrap_or(Value::Null)}),
                    "effect_id",
                    effect_id.map(Value::String),
                ),
                None,
            ),
            Self::Schedules(Schedules::Create {
                name,
                work_id,
                cron,
                tz,
                params,
                tags,
            }) => (
                "schedule",
                with_optional(
                    with_optional(
                        json!({"name":name,"work_id":work_id,"cron":cron,"tz":tz}),
                        "params",
                        params.as_deref().map(json_input).transpose()?,
                    ),
                    "tags",
                    (!tags.is_empty()).then(|| json!(tags)),
                ),
                None,
            ),
            Self::Schedules(Schedules::Edit {
                name,
                cron,
                tz,
                params,
                clear_params,
                enable,
                disable,
                tags,
                clear_tags,
            }) => {
                let mut body = json!({"name":name});
                if let Some(c) = cron {
                    body["cron"] = json!(c);
                }
                if let Some(z) = tz {
                    body["tz"] = json!(z);
                }
                if let Some(path) = params {
                    body["params"] = json_input(&path)?;
                } else if clear_params {
                    body["params"] = Value::Null;
                }
                if enable || disable {
                    body["enabled"] = json!(enable);
                }
                if clear_tags {
                    body["tags"] = json!([]);
                } else if !tags.is_empty() {
                    body["tags"] = json!(tags);
                }
                ("schedule_update", body, None)
            }
            Self::Schedules(Schedules::Pause { name }) => (
                "schedule_action",
                json!({"name":name,"action":"pause"}),
                None,
            ),
            Self::Schedules(Schedules::Resume { name }) => (
                "schedule_action",
                json!({"name":name,"action":"resume"}),
                None,
            ),
            Self::Schedules(Schedules::Delete { name }) => (
                "schedule_action",
                json!({"name":name,"action":"delete"}),
                None,
            ),
            Self::Schedules(Schedules::Occurrences { name, limit }) => (
                "schedule_occurrences",
                with_optional(json!({"name":name}), "limit", limit.map(|l| json!(l))),
                None,
            ),
        };
        let params_copy = params.clone();
        let result = client.invoke(method, params).await?;
        if method == "definitions_import" {
            let count = |kind: &str, action: &str| {
                result[kind]
                    .as_array()
                    .map_or(0, |a| a.iter().filter(|i| i["action"] == action).count())
            };
            let issues = result["issues"].as_array().map_or(0, Vec::len);
            eprintln!(
                "{}: work {} create, {} replace, {} unchanged; schedules {} create, {} edit, {} unchanged; {} problem(s)",
                if result["applied"] == true {
                    "Imported"
                } else if issues > 0 {
                    "Refused (nothing written)"
                } else {
                    "Dry run (nothing written)"
                },
                count("work", "create"),
                count("work", "replace"),
                count("work", "unchanged") + count("work", "set_enabled"),
                count("schedules", "create"),
                count("schedules", "edit"),
                count("schedules", "unchanged"),
                issues
            );
        }
        if let Some(note) = crate::cli_views::schedule_mutation_note(method, &params_copy, &result)
        {
            eprintln!("{note}");
        }
        let result = match projection {
            Some(field) => result
                .get(field)
                .cloned()
                .ok_or_else(|| failure("Forge snapshot is missing the requested collection"))?,
            None => result,
        };
        println!("{}", serde_json::to_string_pretty(&result)?);
        if method == "definitions_import"
            && result["issues"].as_array().is_some_and(|i| !i.is_empty())
        {
            return Err(crate::client::ExitStatus(1).into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_input_rejects_overflow_invalid_and_trailing_data() {
        assert!(read_json(&b"{} {}"[..]).is_err());
        assert!(
            !read_json(&b"secret invalid"[..])
                .unwrap_err()
                .to_string()
                .contains("secret")
        );
        assert!(
            read_json(vec![b' '; MAX_REQUEST + 1].as_slice())
                .unwrap_err()
                .to_string()
                .contains("1 MiB")
        );
        assert_eq!(
            read_json(&b"{\"type\":\"echo\",\"input\":{}}"[..]).unwrap()["type"],
            "echo"
        );
    }
}

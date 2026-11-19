use crate::{
    dbos::{
        RuntimeConfig,
        admission::{AdmissionStore, Envelope, Invocation, ScopedReference, SubmissionBridge},
        effects::EffectAccess,
    },
    plugin::{Plugin, PluginConfig},
    safety,
    settings::{self, Paths, Settings},
};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};
pub const WORKFLOW: &str = "forge_execute_v1";
pub const SCHEDULED: &str = "forge_scheduled_v1";
pub const VERSION: &str = "forge-rust-mvp-v1";
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Denied(pub &'static str);
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Invalid(pub &'static str);
/// A refused flow definition with a bounded, value-free location and reason.
#[derive(Debug, thiserror::Error)]
#[error("invalid flow definition: {0}")]
pub struct InvalidFlow(pub String);
#[derive(Debug, thiserror::Error, Serialize, Deserialize)]
#[error("Execution refused or failed; inspect scoped step and effect evidence")]
pub struct WorkFault;
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    pub token: String,
    pub capabilities: Vec<String>,
    pub types: Vec<String>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub key_env: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub home_id: String,
    pub actors: BTreeMap<String, Actor>,
    #[serde(default)]
    pub models: BTreeMap<String, Model>,
    #[serde(default)]
    pub plugins: BTreeMap<String, PluginConfig>,
    /// Operator-approved native harness runners; workflows select them only by name.
    #[serde(default)]
    pub runners: BTreeMap<String, crate::harness::RunnerProfile>,
    /// Optional runtime sections; absent sections use the resolver defaults.
    #[serde(default, skip_serializing_if = "is_default")]
    pub database: settings::DatabaseConfig,
    #[serde(default, skip_serializing_if = "is_default")]
    pub paths: settings::PathsConfig,
    #[serde(default, skip_serializing_if = "is_default")]
    pub server: settings::ServerConfig,
    #[serde(default, skip_serializing_if = "is_default")]
    pub dbos: settings::DbosConfig,
    #[serde(default, skip_serializing_if = "is_default")]
    pub harness: settings::HarnessConfig,
    #[serde(default, skip_serializing_if = "is_default")]
    pub containment: settings::ContainmentConfig,
}
fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduleContext {
    pub home_id: String,
    pub actor: String,
    pub work_id: String,
    pub name: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::canonical::serialize_option"
    )]
    pub params: Option<Value>,
}
pub fn hash(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}
/// Admission request identity of `key` for one work item.
pub fn request_id(work_id: &str, key: &str) -> Result<String> {
    Ok(hash(crate::canonical::to_vec(&json!([work_id, key]))?))
}
/// Deterministic idempotency key a schedule's occurrence due at `due` admits with.
pub fn schedule_key(name: &str, due: std::time::SystemTime) -> Result<String> {
    let time = due
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| Invalid("scheduled time precedes epoch"))?
        .as_millis();
    Ok(hash(crate::canonical::to_vec(&json!([
        "schedule", name, time
    ]))?))
}
/// Whether a native firing of `name` is an explicit manual trigger. DBOS names a
/// trigger `sched-<name>-trigger-<time>` and a due tick `sched-<name>-<time>`; the
/// exact full schedule name is compared, never split or suffix-matched.
pub fn manual_trigger(name: &str, occurrence_id: &str) -> bool {
    occurrence_id
        .strip_prefix("sched-")
        .and_then(|rest| rest.strip_prefix(name))
        .is_some_and(|rest| rest.starts_with("-trigger-"))
}
/// The admission key of one native firing. Due ticks keep their deterministic
/// per-tick key; each manual trigger is keyed by its own native trigger id, so two
/// triggers are always two runs and a replayed handoff admits the same run again.
pub fn occurrence_key(
    name: &str,
    due: std::time::SystemTime,
    occurrence_id: Option<&str>,
) -> Result<String> {
    match occurrence_id {
        Some(id) if manual_trigger(name, id) => Ok(hash(crate::canonical::to_vec(&json!([
            "schedule-trigger",
            name,
            id
        ]))?)),
        _ => schedule_key(name, due),
    }
}
pub fn identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err(Invalid("use a bounded alphanumeric identifier").into());
    }
    Ok(())
}
/// Every check a policy must pass to load. Messages never contain values.
pub fn validate_policy(home: &Path, policy: &Policy) -> Result<()> {
    // Reject malformed runtime sections at load time; the database itself is only
    // required by commands that connect.
    let resolved = Settings::resolve(home.to_path_buf(), policy, &Default::default(), &|_| None)?;
    for (name, actor) in &policy.actors {
        identifier(name)?;
        if actor.token.len() < 24 || actor.token.len() > 512 {
            return settings::refuse(format!(
                "actor {name}: tokens must be private high-entropy values (24 to 512 characters)"
            ));
        }
    }
    for (name, plugin) in &policy.plugins {
        if plugin.env_passthrough.len() > 32
            || plugin.env_passthrough.iter().any(|v| {
                !crate::plugin::passthrough_allowed(v)
                    || policy.database.url_env.as_deref() == Some(v.as_str())
            })
        {
            return Err(settings::ConfigError(format!(
                "plugin {name} env_passthrough may name only ordinary variables, never FORGE_* settings or the database credential variable"
            ))
            .into());
        }
    }
    for (name, runner) in &policy.runners {
        // Validation messages never contain values, so they are safe to show verbatim.
        runner
            .validate_for_paths(name, &resolved.paths)
            .map_err(|e| settings::ConfigError(format!("runner profile {name} refused: {e}")))?;
    }
    for (name, model) in &policy.models {
        validate_model(name, model)?;
    }
    Ok(())
}
/// An approved model alias: an explicit OpenAI-compatible endpoint with, at most, the
/// NAME of an environment variable that holds its credential.
pub fn validate_model(name: &str, model: &Model) -> Result<()> {
    identifier(name)?;
    let refused = |why: &str| -> anyhow::Error {
        settings::ConfigError(format!("model alias {name} refused: {why}")).into()
    };
    let url =
        reqwest::Url::parse(&model.base_url).map_err(|_| refused("base_url is not a valid URL"))?;
    if !["http", "https"].contains(&url.scheme())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || model.model.is_empty()
    {
        return Err(refused(
            "an explicit http(s) endpoint without credentials, query or fragment and a model name is required",
        ));
    }
    if let Some(key) = &model.key_env {
        if key.is_empty()
            || key.len() > 128
            || key.as_bytes()[0].is_ascii_digit()
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(refused("key_env must be an environment variable name"));
        }
        if url.scheme() != "https"
            && !matches!(
                url.host_str(),
                Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
            )
        {
            return Err(refused("authenticated remote models require HTTPS"));
        }
    }
    Ok(())
}
pub fn load_policy(home: &Path) -> Result<Policy> {
    if settings::restore_marker(home).exists() {
        return settings::refuse("restore incomplete; finish recovery before startup");
    }
    let bytes = safety::read_private(&settings::config_file(home)).map_err(|e| {
        settings::ConfigError(format!(
            "config.json cannot be read as one private regular file owned by you ({}); run `agent-forge init` for a new home or restore its 0600 mode",
            e.kind()
        ))
    })?;
    let policy: Policy = serde_json::from_slice(&bytes).map_err(settings::config_parse_error)?;
    validate_policy(home, &policy)?;
    Ok(policy)
}
pub fn init(home: &Path) -> Result<()> {
    if settings::restore_marker(home).exists() {
        return settings::refuse("restore incomplete; finish recovery before init");
    }
    if settings::config_file(home).exists() {
        load_policy(home)?;
        return Ok(());
    }
    // Only the home and its config are created here. The state directory (and its
    // owner lock) belong to the layout the service resolves from config.json/env at
    // `serve`, so init never leaves a stray state/owner.lock at a location a config
    // later moves the state away from. Two concurrent inits are fenced by `write_new`
    // (create-new), and a service cannot run before config.json exists.
    safety::private_mkdir(home)?;
    let mut id = [0u8; 16];
    getrandom::getrandom(&mut id).map_err(|_| anyhow!("random source unavailable"))?;
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key).map_err(|_| anyhow!("random source unavailable"))?;
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let policy = json!({"home_id":hex(&id),"actors":{"operator":{"token":hex(&key),"capabilities":["read","register","execute","schedule","reconcile","plugins","approve","configure","transfer"],"types":["echo","fail","model","agent","workflow","harness","approval"]}},"models":{},"plugins":{},"runners":{}});
    safety::write_new(
        &settings::config_file(home),
        serde_json::to_string_pretty(&policy)?.as_bytes(),
    )?;
    Ok(())
}
pub struct Core {
    pub home: PathBuf,
    pub paths: Paths,
    pub policy: Policy,
    pub config: RuntimeConfig,
    pub store: Arc<AdmissionStore>,
    pub plugins: BTreeMap<String, Plugin>,
    /// Installed (manifest) plugins and their lifecycle state.
    pub(crate) registry: Arc<crate::plugin_registry::Registry>,
    pub bridge: OnceLock<Arc<SubmissionBridge>>,
    pub client: OnceLock<dbos::Client>,
    /// The settings resolved at startup, with the source of each value.
    pub settings: OnceLock<Settings>,
    /// Serializes read-modify-write edits of `config.json`.
    pub(crate) config_lock: std::sync::Mutex<()>,
    pub(crate) schedule_locks: crate::schedule_edit::ScheduleLocks,
    /// The latest online backup started through the service.
    pub(crate) backup_job: std::sync::Mutex<Option<Value>>,
    pub(crate) probes:
        std::sync::Mutex<BTreeMap<String, (std::time::Instant, crate::harness::Probe)>>,
}
impl Core {
    pub fn new(
        paths: Paths,
        policy: Policy,
        config: RuntimeConfig,
        store: Arc<AdmissionStore>,
        plugins: BTreeMap<String, Plugin>,
    ) -> Result<Arc<Self>> {
        safety::private_mkdir(&paths.outputs)?;
        safety::private_mkdir(&paths.effect_locks)?;
        store.write(|c|{c.execute_batch("CREATE TABLE IF NOT EXISTS forge_work(id TEXT PRIMARY KEY,owner TEXT NOT NULL,definition TEXT NOT NULL,enabled INTEGER NOT NULL,created REAL NOT NULL);CREATE TABLE IF NOT EXISTS forge_audit(id INTEGER PRIMARY KEY,actor TEXT,action TEXT,target TEXT,detail TEXT,created REAL);CREATE TABLE IF NOT EXISTS forge_schedules(name TEXT PRIMARY KEY,context TEXT NOT NULL,cron TEXT NOT NULL,tz TEXT NOT NULL);CREATE TABLE IF NOT EXISTS forge_inputs(digest TEXT PRIMARY KEY,payload TEXT NOT NULL);CREATE TABLE IF NOT EXISTS forge_run_fences(run_id TEXT PRIMARY KEY);CREATE TABLE IF NOT EXISTS forge_plugins(id TEXT PRIMARY KEY,version TEXT NOT NULL,state TEXT NOT NULL,manifest TEXT NOT NULL,manifest_sha256 TEXT NOT NULL,installed REAL NOT NULL);CREATE TABLE IF NOT EXISTS forge_plugin_resources(work_id TEXT PRIMARY KEY,plugin_id TEXT NOT NULL,kind TEXT NOT NULL,resource_id TEXT NOT NULL,schedule_name TEXT);CREATE TABLE IF NOT EXISTS forge_flow_effects(run_id TEXT NOT NULL,slot TEXT NOT NULL,invocation_digest TEXT NOT NULL,action_digest TEXT NOT NULL,state TEXT NOT NULL,receipt TEXT,action TEXT NOT NULL,decision TEXT,PRIMARY KEY(run_id,slot));")?;c.execute_batch(crate::approvals::SCHEMA)?;c.execute_batch(crate::lost_runs::SCHEMA)?;crate::schedule_edit::migrate(c)?;Ok(())})?;
        Ok(Arc::new(Self {
            home: paths.home.clone(),
            paths,
            policy,
            config,
            store,
            plugins,
            registry: Default::default(),
            bridge: OnceLock::new(),
            client: OnceLock::new(),
            settings: OnceLock::new(),
            config_lock: Default::default(),
            schedule_locks: Default::default(),
            backup_job: Default::default(),
            probes: Default::default(),
        }))
    }
    /// Before DBOS recovery, terminate plugin invocations still bound to an
    /// unresolved existing effect intent. The effect ledger remains the sole
    /// recovery authority: this only removes a surviving process tree and never
    /// retries or settles the external outcome.
    pub fn reconcile_plugin_effects(&self) -> Result<()> {
        let ordinary = self.store.write(|db| {
            let mut statement = db.prepare(
                "SELECT effect_key FROM dbos_effect_receipts WHERE state='uncertain' ORDER BY effect_key",
            )?;
            Ok(statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        for key in ordinary {
            crate::plugin::reconcile(&key)?;
        }
        let flow = self.store.write(|db| {
            let mut statement = db.prepare(
                "SELECT run_id,slot,action_digest FROM forge_flow_effects WHERE state='uncertain' ORDER BY run_id,slot",
            )?;
            Ok(statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        for (run_id, slot, action_digest) in flow {
            let key = format!(
                "forge-flow-effect-{}",
                hash(crate::canonical::to_vec(&json!([
                    run_id,
                    slot,
                    action_digest
                ]))?),
            );
            crate::plugin::reconcile(&key)?;
        }
        Ok(())
    }
    pub fn actor(&self, token: &str) -> Result<String> {
        for (name, a) in &self.policy.actors {
            if a.token.len() == token.len()
                && a.token
                    .bytes()
                    .zip(token.bytes())
                    .fold(0u8, |x, (a, b)| x | (a ^ b))
                    == 0
            {
                return Ok(name.clone());
            }
        }
        Err(Denied("unknown or missing bearer credential").into())
    }
    pub fn permit(&self, actor: &str, capability: &str, kind: Option<&str>) -> Result<()> {
        let a = self
            .policy
            .actors
            .get(actor)
            .ok_or(Denied("actor unavailable"))?;
        let missing = if !a.capabilities.iter().any(|c| c == capability) {
            Some(format!("capability `{capability}`"))
        } else {
            kind.filter(|k| !a.types.iter().any(|t| t == k))
                .map(|k| format!("work type `{k}`"))
        };
        if let Some(missing) = missing {
            // The typed `Denied` stays the root cause (it maps to 403); the context is the
            // message clients see, naming exactly what is missing and where to look.
            return Err(anyhow::Error::new(Denied("operation outside actor authority")).context(format!(
                "operation outside actor authority: actor {actor} lacks {missing}. Run `agent-forge doctor` for the exact config.json change (the operator edits the actor's capabilities/types and restarts)"
            )));
        }
        Ok(())
    }
    pub fn record(&self, actor: &str, action: &str, target: &str, detail: &str) -> Result<()> {
        Self::record_at(
            &self.store,
            actor,
            action,
            target,
            detail,
            Utc::now().timestamp_millis(),
        )
    }

    pub(crate) fn record_at(
        store: &AdmissionStore,
        actor: &str,
        action: &str,
        target: &str,
        detail: &str,
        created_ms: i64,
    ) -> Result<()> {
        store.write(|c| {
            c.execute("INSERT INTO forge_audit(actor,action,target,detail,created) VALUES(?1,?2,?3,?4,?5)",
                params![actor, action, target, detail, created_ms as f64 / 1000.])?;
            Ok(())
        })?;
        Ok(())
    }
    pub fn work(&self, actor: &str, id: &str) -> Result<Value> {
        let row = self.store.write(|c| {
            Ok(c.query_row(
                "SELECT owner,definition,enabled,created FROM forge_work WHERE id=?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, bool>(2)?,
                        r.get::<_, f64>(3)?,
                    ))
                },
            )
            .optional()?)
        })?;
        let (owner, definition, enabled, created) =
            row.ok_or(Denied("work missing or outside your scope"))?;
        if owner != actor {
            return Err(Denied("work missing or outside your scope").into());
        }
        Ok(
            json!({"id":id,"owner":owner,"definition":serde_json::from_str::<Value>(&definition)?,"enabled":enabled,"created":created}),
        )
    }
    pub fn admitted(&self, actor: &str, id: &str) -> Result<Value> {
        // Dispatch-time (not only UI) gate: plugin-owned work needs its plugin enabled.
        self.plugin_work_gate(id)?;
        let w = self.work(actor, id)?;
        if w["enabled"] != true {
            return Err(Denied("work disabled").into());
        }
        self.permit(actor, "execute", w["definition"]["type"].as_str())?;
        Ok(w["definition"].clone())
    }
    pub fn types(&self) -> Value {
        let mut v = json!({"echo":{"description":"Return explicit text after an optional durable delay","schema":{"type":"object","properties":{"text":{"type":"string","maxLength":65536},"delay":{"type":"number","minimum":0,"maximum":300}},"required":["text"],"additionalProperties":false}},"fail":{"description":"Deliberate controlled failure","schema":{"type":"object","properties":{},"additionalProperties":false}},"approval":{"description":"Workflow step only: wait durably for an authorized actor to approve or reject (optional timeout); rejection or timeout fails the step","schema":{"type":"object","properties":{"title":{"type":"string","minLength":1,"maxLength":200},"description":{"type":"string","maxLength":4096},"timeout_seconds":{"type":"integer","minimum":1,"maximum":604800}},"required":["title"],"additionalProperties":false}},"model":{"description":"Explicit configured model invocation","schema":{"type":"object","properties":{"model":{"type":"string"},"prompt":{"type":"string","minLength":1,"maxLength":65536},"expect":{"type":"string","maxLength":4096}},"required":["model","prompt"],"additionalProperties":false}}});
        if let Some(catalog) = crate::flow_schema::catalog().as_object() {
            for (name, entry) in catalog {
                v[name] = entry.clone();
            }
        }
        v["harness"] = crate::flow_schema::harness_catalog();
        for (name, p) in &self.plugins {
            v[name] = json!({"description":p.description,"schema":p.schema});
        }
        // Installed plugins appear only while enabled; a disabled one is not a type.
        for (name, p) in self.installed_types() {
            v[name] = json!({"description":p.description,"schema":p.schema});
        }
        v
    }
    pub fn register(&self, actor: &str, id: &str, definition: Value) -> Result<Value> {
        identifier(id)?;
        if id.starts_with("plugin.") {
            return Err(
                Invalid("work ids beginning plugin. are reserved for installed plugins").into(),
            );
        }
        self.check_definition(actor, &definition)?;
        let raw = crate::canonical::to_string(&definition)?;
        self.store_definition(actor, id, raw)
    }
    /// Every registration check, without persistence: shape, authority, input
    /// schema, configured destinations and flow references/limits.
    pub fn check_definition(&self, actor: &str, definition: &Value) -> Result<()> {
        let d = definition
            .as_object()
            .ok_or(Invalid("definition must be an object"))?;
        if d.len() != 2 || !d.contains_key("input") {
            return Err(Invalid("definition requires only type and input").into());
        }
        let kind = d
            .get("type")
            .and_then(Value::as_str)
            .ok_or(Invalid("work type missing"))?;
        // An unknown type is invalid input, not an authority gap: never advise granting it.
        if !self
            .types()
            .as_object()
            .is_some_and(|t| t.contains_key(kind))
        {
            return Err(Invalid("unknown work type").into());
        }
        self.permit(actor, "register", Some(kind))?;
        if kind == "approval" {
            return Err(Invalid("approval is a workflow step, not standalone work").into());
        }
        let types = self.types();
        let schema = types.get(kind).ok_or(Invalid("unknown work type"))?["schema"].clone();
        crate::work_input::validate(&schema, &d["input"])?;
        if kind == "model"
            && !self
                .policy
                .models
                .contains_key(d["input"]["model"].as_str().unwrap_or(""))
        {
            return Err(Invalid("model alias is not configured").into());
        }
        self.validate_definition(actor, definition, "register")?;
        Ok(())
    }
    fn store_definition(&self, actor: &str, id: &str, raw: String) -> Result<Value> {
        self.store.write(|c| {
            let old = c
                .query_row(
                    "SELECT owner,definition FROM forge_work WHERE id=?1",
                    [id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?;
            if let Some((a, d)) = old {
                if a != actor || d != raw {
                    return Err(crate::dbos::admission::Error::Invalid(
                        "work identity already exists with different immutable scope or definition",
                    ));
                }
            } else {
                c.execute(
                    "INSERT INTO forge_work VALUES(?1,?2,?3,1,?4)",
                    params![id, actor, raw, Utc::now().timestamp_millis() as f64 / 1000.],
                )?;
            }
            Ok(())
        })?;
        self.record(actor, "register", id, "")?;
        self.work(actor, id)
    }
    pub fn enable(&self, actor: &str, id: &str, enabled: bool) -> Result<Value> {
        let w = self.work(actor, id)?;
        self.permit(actor, "register", w["definition"]["type"].as_str())?;
        self.store.write(|c| {
            c.execute(
                "UPDATE forge_work SET enabled=?2 WHERE id=?1",
                params![id, enabled],
            )?;
            Ok(())
        })?;
        self.record(actor, "enable", id, if enabled { "true" } else { "false" })?;
        self.work(actor, id)
    }
    fn envelope(
        &self,
        actor: &str,
        id: &str,
        key: &str,
        parent: Option<&str>,
        input: Option<Value>,
    ) -> Result<Envelope> {
        let definition = self.admitted(actor, id)?;
        if !crate::flow_core::accepts_params(&definition)
            && input
                .as_ref()
                .is_some_and(|v| !v.is_object() || v.as_object().is_some_and(|o| !o.is_empty()))
        {
            return Err(
                Invalid("root invocation params require agent, workflow or harness work").into(),
            );
        }
        let mut params = parent
            .map(|p| json!({"parent":{"scope":actor,"object_id":p,"sha256":hash(p)}}))
            .unwrap_or_else(|| json!({}));
        if let Some(input) = input {
            params["input"] = self.params_reference(actor, &input)?;
        }
        let inv=Invocation::from_json(&json!({"format_version":1,"home_id":self.policy.home_id,"scope":actor,"workflow_name":WORKFLOW,"request_id":request_id(id,key)?,"workflow_version":VERSION,"principal":actor,"workspace":id,"plugin":definition["type"],"resource":"bounded-work-queue","policy_ref":"configured-actor","definition_sha256":hash(crate::canonical::to_vec(&definition)?),"action_sha256":self.action_hash(actor, &definition)?,"parameters":params,"evidence":[],"authorization_expires_at_ms":null}).to_string())?;
        Ok(Envelope::new(inv))
    }
    pub async fn run(
        &self,
        actor: &str,
        id: &str,
        key: &str,
        parent: Option<&str>,
    ) -> Result<Value> {
        self.run_with_params(actor, id, key, parent, None).await
    }
    pub async fn run_with_params(
        &self,
        actor: &str,
        id: &str,
        key: &str,
        parent: Option<&str>,
        input: Option<Value>,
    ) -> Result<Value> {
        if key.is_empty() || key.len() > 180 || key.chars().any(char::is_control) {
            return Err(Invalid("bounded idempotency key required").into());
        }
        let env = self.envelope(actor, id, key, parent, input)?;
        self.store.admit(&env.invocation)?;
        self.record(actor, "run", &env.dbos_id, "")?;
        self.bridge
            .get()
            .context("executor not launched")?
            .submit(&env)
            .await?;
        Ok(json!({"run_id":env.dbos_id}))
    }
    pub fn owned(&self, actor: &str, id: &str) -> Result<Envelope> {
        self.permit(actor, "read", None)?;
        let row = self
            .store
            .inspect(id)?
            .ok_or(Denied("run missing or outside your scope"))?;
        if row.envelope.invocation.principal() != actor {
            return Err(Denied("run missing or outside your scope").into());
        }
        Ok(row.envelope)
    }
    pub fn current(&self, e: &Envelope) -> Result<Value> {
        self.store.verify(e)?;
        let raw = e.invocation.source_value();
        let id = raw["workspace"]
            .as_str()
            .ok_or(Invalid("work binding missing"))?;
        let definition = self.admitted(e.invocation.principal(), id)?;
        if hash(crate::canonical::to_vec(&definition)?) != raw["definition_sha256"]
            || self.action_hash(e.invocation.principal(), &definition)?
                != e.invocation.action_sha256()
        {
            return Err(Denied("admitted definition or destination changed").into());
        }
        Ok(definition)
    }
    pub fn effect_lock(&self, id: &str) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.paths.effect_locks.join(hash(id)))?;
        // A concurrent fork in another thread can briefly hold a copy of a just-released
        // descriptor until its exec closes it, so a short bounded retry is not a real conflict.
        let mut locked = false;
        for attempt in 0..40 {
            if unsafe {
                libc::flock(
                    std::os::fd::AsRawFd::as_raw_fd(&file),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            } == 0
            {
                locked = true;
                break;
            }
            if attempt < 39 {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        if !locked {
            return Err(
                Invalid("external operation is still active; outcome must settle first").into(),
            );
        }
        Ok(file)
    }
    fn output_path(&self, id: &str) -> PathBuf {
        self.paths.outputs.join(format!("{}.json", hash(id)))
    }
    pub fn receipt(&self, e: &Envelope, output: Value) -> Result<ScopedReference> {
        let bytes = crate::canonical::to_vec(
            &json!({"run_id":e.dbos_id,"action_sha256":e.invocation.action_sha256(),"output":output}),
        )?;
        let path = self.output_path(&e.dbos_id);
        if path.exists() {
            if safety::read_private(&path)? != bytes {
                bail!("immutable output receipt conflict");
            }
        } else {
            safety::write_new(&path, &bytes)?;
        }
        Ok(ScopedReference {
            scope: e.invocation.scope().into(),
            object_id: format!("output:{}", hash(&e.dbos_id)),
            sha256: hash(bytes),
        })
    }
    pub fn read_output(&self, e: &Envelope, r: &ScopedReference) -> Result<Value> {
        r.validate(e.invocation.scope())?;
        if r.object_id != format!("output:{}", hash(&e.dbos_id)) {
            bail!("output receipt scope mismatch");
        }
        let bytes = safety::read_private(&self.output_path(&e.dbos_id))?;
        if hash(&bytes) != r.sha256 {
            bail!("output receipt integrity failed");
        }
        let v: Value = serde_json::from_slice(&bytes)?;
        if v["run_id"] != e.dbos_id || v["action_sha256"] != e.invocation.action_sha256() {
            bail!("output receipt identity mismatch");
        }
        Ok(v["output"].clone())
    }
    pub async fn execute_effect(&self, e: &Envelope, definition: &Value) -> Result<Value> {
        let _guard = self.effect_lock(&e.dbos_id)?;
        let access = self.store.effect(
            e,
            e.invocation.action_sha256(),
            |_, _| {
                self.current(e).map(|_| ()).map_err(|_| {
                    crate::dbos::admission::Error::Invalid("current execution authority refused")
                })
            },
            |r| {
                self.read_output(e, r).map(|_| ()).map_err(|_| {
                    crate::dbos::admission::Error::Invalid("receipt integrity refused")
                })
            },
        )?;
        let output = match access {
            EffectAccess::Verified(r) => self.read_output(e, &r)?,
            EffectAccess::Execute(permit) => {
                let outbound = permit.before_outbound(|_, _| {
                    self.current(e).map(|_| ()).map_err(|_| {
                        crate::dbos::admission::Error::Invalid(
                            "current authority refused before outbound",
                        )
                    })
                })?;
                self.record("runtime", "effect_intent", &e.dbos_id, "")?;
                let kind = definition["type"].as_str().unwrap_or("");
                let data = &definition["input"];
                let output = if kind == "model" {
                    let model = self
                        .policy
                        .models
                        .get(data["model"].as_str().unwrap_or(""))
                        .context("model unavailable")?;
                    let client = reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .timeout(Duration::from_secs(90))
                        .build()?;
                    let mut request=client.post(format!("{}/chat/completions",model.base_url.trim_end_matches('/'))).json(&json!({"model":model.model,"messages":[{"role":"user","content":data["prompt"]}],"stream":false,"max_tokens":256,"temperature":0,"chat_template_kwargs":{"enable_thinking":false}}));
                    if let Some(key) = &model.key_env {
                        request = request.bearer_auth(
                            std::env::var(key).context("model credential unavailable")?,
                        );
                    }
                    let mut response = request.send().await?.error_for_status()?;
                    let mut bytes = Vec::new();
                    while let Some(chunk) = response.chunk().await? {
                        if bytes.len() + chunk.len() > 1024 * 1024 {
                            bail!("model response exceeded limit");
                        }
                        bytes.extend(chunk);
                    }
                    let response: Value = serde_json::from_slice(&bytes)?;
                    let text = response["choices"][0]["message"]["content"]
                        .as_str()
                        .context("model reply malformed")?;
                    json!({"text":text,"model":response["model"],"artifacts":[]})
                } else {
                    let p = self.plugin(kind).context("plugin unavailable")?;
                    let value = crate::plugin::invoke(
                        &p.config,
                        p.execute_request(data, &e.dbos_id, outbound.idempotency_key()),
                    )
                    .await?;
                    value
                        .get("output")
                        .cloned()
                        .context("plugin output missing")?
                };
                let receipt = self.receipt(e, output.clone())?;
                outbound.complete(receipt, |r| {
                    self.read_output(e, r).map(|_| ()).map_err(|_| {
                        crate::dbos::admission::Error::Invalid(
                            "completed receipt verification failed",
                        )
                    })
                })?;
                self.record("runtime", "effect_completed", &e.dbos_id, "")?;
                output
            }
        };
        self.current(e)?;
        Ok(output)
    }
}
impl Core {
    pub(crate) fn native(&self) -> Result<&dbos::Client> {
        self.client.get().context("executor not available")
    }
    pub async fn inspect(&self, actor: &str, id: &str) -> Result<Value> {
        self.inspect_selected(actor, id, None, None).await
    }
    /// Canonical run inspection, redacted exactly like run logs and the live stream:
    /// configured actor tokens, model and runner secrets and token-shaped strings never
    /// leave through `inspect` (outputs, effect receipts and step records included).
    pub async fn inspect_selected(
        &self,
        actor: &str,
        id: &str,
        effect_id: Option<&str>,
        step_id: Option<i64>,
    ) -> Result<Value> {
        let run = self
            .inspect_unredacted(actor, id, effect_id, step_id)
            .await?;
        Ok(self.redact_value(&run))
    }
    async fn inspect_unredacted(
        &self,
        actor: &str,
        id: &str,
        effect_id: Option<&str>,
        step_id: Option<i64>,
    ) -> Result<Value> {
        if effect_id.is_some() && step_id.is_some() {
            return Err(Invalid("select effect_id or step_id, not both").into());
        }
        let e = self.owned(actor, id)?;
        let iv = e.invocation.source_value();
        let work = self.work(
            actor,
            iv["workspace"]
                .as_str()
                .ok_or(Invalid("work binding missing"))?,
        )?;
        let rows = self
            .native()?
            .list_workflows(&dbos::sysdb::types::WorkflowFilter {
                workflow_ids: vec![id],
                load_input: false,
                load_output: true,
                ..Default::default()
            })
            .await?;
        let row = rows.first();
        let mut steps=self.native()?.list_workflow_steps(id).await?.into_iter().map(|s|json!({"function_id":s.step_id,"function_name":s.step_name,"started_at":s.started_at,"completed_at":s.completed_at,"output":s.output.and_then(|o|serde_json::from_str::<Value>(&o).ok()),"error":s.error.map(|_|"WorkFault")})).collect::<Vec<_>>();
        let logs=self.store.write(|c|{let mut stmt=c.prepare("SELECT actor,action,detail,created FROM (SELECT id,actor,action,detail,created FROM forge_audit WHERE target=?1 ORDER BY id DESC LIMIT 100) ORDER BY id")?;let rows=stmt.query_map([id],|r|Ok(json!({"actor":r.get::<_,String>(0)?,"action":r.get::<_,String>(1)?,"detail":r.get::<_,String>(2)?,"created":r.get::<_,f64>(3)?})))?;Ok(rows.collect::<std::result::Result<Vec<_>,_>>()?)})?;
        let mut effect = self.store.effect_observation(&e)?;
        if let Some(value) = &mut effect {
            value["reconciliation"] = self.store.effect_decision(&e)?.unwrap_or(Value::Null);
            if value["state"] == "completed" {
                let receipt: ScopedReference = serde_json::from_value(value["receipt"].clone())?;
                value["output"] = self.read_output(&e, &receipt)?;
            }
        }
        let mut effects = self.flow_effect_observations(&e)?;
        let lost = if row.is_none() {
            self.lost_run(id)?
        } else {
            None
        };
        let status = match &lost {
            Some(l) => match l.decision.as_deref() {
                None => "RECOVERY_REQUIRED",
                Some("completed") => "SUCCESS",
                Some(_) => "ERROR",
            },
            None => row.map(|r| r.status.as_str()).unwrap_or("ADMITTED"),
        };
        let active = ["PENDING", "ENQUEUED", "DELAYED"].contains(&status);
        let unknown = effect.as_ref().is_some_and(|x| x["state"] == "uncertain")
            || effects.iter().any(|x| x["state"] == "uncertain");
        let diagnostic = if let Some(l) = &lost {
            match l.decision.as_deref() {
                None => {
                    "This run was admitted but its native workflow does not exist (an inconsistent or restored database). Forge will not replay it: reconcile it with evidence (completed or not_applied)."
                }
                Some("completed") => {
                    "Reconciled as completed by an operator; no workflow ran in this database."
                }
                Some(_) => "Reconciled as not applied; an explicit linked retry is permitted.",
            }
        } else if unknown {
            if active {
                "External effect in progress; cancellation cannot undo it."
            } else {
                "External effect may have happened. Reconcile settled provider evidence before retry."
            }
        } else if (effect.as_ref().is_some_and(|x| x["state"] == "completed")
            || effects.iter().any(|x| x["state"] == "completed"))
            && status != "SUCCESS"
        {
            "External effect completed; replay prohibited. Inspect recorded output."
        } else if status == "ERROR" {
            "Definite failure; inspect the step timeline and permitted actions."
        } else {
            ""
        };
        if let Some(selected) = effect_id {
            let effect = effects
                .into_iter()
                .find(|v| v["effect_id"] == selected)
                .ok_or(Denied("effect missing or outside run"))?;
            return Ok(
                json!({"run_id":id,"work_id":work["id"],"status":if unknown&&!active{"RECOVERY_REQUIRED"}else{status},"effect":effect}),
            );
        }
        if let Some(selected) = step_id {
            let step = steps
                .into_iter()
                .find(|v| v["function_id"] == selected)
                .ok_or(Denied("step missing or outside run"))?;
            return Ok(
                json!({"run_id":id,"work_id":work["id"],"status":if unknown&&!active{"RECOVERY_REQUIRED"}else{status},"step":step}),
            );
        }
        for step in &mut steps {
            step["output"] = crate::inspection::preview(&step["output"]);
        }
        for effect in &mut effects {
            effect["output"] = crate::inspection::preview(&effect["output"]);
            effect["action"] = crate::inspection::preview(&effect["action"]);
        }
        let mut run = json!({"run_id":id,"work_id":work["id"],"definition":crate::inspection::preview(&work["definition"]),"summary":true,"parent":iv["parameters"]["parent"]["object_id"],"created":row.map(|r|r.created_at),"status":if unknown&&!active{"RECOVERY_REQUIRED"}else{status},"output":row.and_then(|r|r.output.as_deref()).and_then(|v|serde_json::from_str::<Value>(v).ok()).or_else(||lost.as_ref().map(|l|l.output.clone()).filter(|o|!o.is_null())),"steps":steps,"logs":logs,"effect":effect,"effects":effects,"params":crate::inspection::preview(&self.invocation_params(&e)?),"error":row.filter(|r|r.error.is_some()).map(|_|"Execution failed; inspect scoped step timeline and effect state."),"diagnostic":diagnostic});
        // Additive operator projections; existing fields keep their meaning.
        run["type"] = work["definition"]["type"].clone();
        run["actions"] = crate::run_views::actions(&run);
        if let Some(workflow) = crate::run_views::workflow_steps(&work["definition"], &run) {
            run["workflow"] = workflow;
        }
        Ok(run)
    }
    pub async fn cancel(&self, actor: &str, id: &str) -> Result<Value> {
        let e = self.owned(actor, id)?;
        self.permit(actor, "execute", None)?;
        if self.lost_run(id)?.is_some() {
            return Err(Invalid(
                "run has no native workflow (inconsistent database): reconcile it instead of cancelling",
            )
            .into());
        }
        if crate::flow_core::is_ledgered(
            &self.work(
                actor,
                e.invocation.source_value()["workspace"]
                    .as_str()
                    .ok_or(Invalid("work missing"))?,
            )?["definition"],
        ) {
            self.fence_flow(&e)?;
        }
        if self
            .native()?
            .workflow_status(id)
            .await?
            .is_some_and(|status| {
                matches!(
                    status,
                    dbos::sysdb::types::WorkflowStatus::Success
                        | dbos::sysdb::types::WorkflowStatus::Error
                        | dbos::sysdb::types::WorkflowStatus::Cancelled
                        | dbos::sysdb::types::WorkflowStatus::MaxRecoveryAttemptsExceeded
                )
            })
        {
            return self.inspect(actor, id).await;
        }
        let kind = e.invocation.source_value()["plugin"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        if (kind == "model" || kind.starts_with("plugin."))
            && let Ok(_guard) = self.effect_lock(id)
        {
            self.store.fence_unstarted(&e)?;
        }
        self.bridge
            .get()
            .context("executor unavailable")?
            .submit(&e)
            .await?;
        self.native()?.cancel(id).await?;
        self.record(
            actor,
            "cancel",
            id,
            "Cooperative cancellation does not undo active external operations",
        )?;
        self.inspect(actor, id).await
    }
    pub async fn retry(&self, actor: &str, id: &str, key: &str) -> Result<Value> {
        let old = self.inspect(actor, id).await?;
        let definition = self.work(
            actor,
            old["work_id"].as_str().ok_or(Invalid("work missing"))?,
        )?["definition"]
            .clone();
        self.permit(actor, "execute", definition["type"].as_str())?;
        if !["ERROR", "CANCELLED", "MAX_RECOVERY_ATTEMPTS_EXCEEDED"]
            .contains(&old["status"].as_str().unwrap_or(""))
        {
            return Err(Invalid("only definite terminal failures permit explicit retry").into());
        }
        let e = self.owned(actor, id)?;
        if crate::flow_core::is_ledgered(&definition) {
            let _guard = self.effect_lock(id)?;
            self.fence_flow(&e)?;
            if !self.flow_effect_observations(&e)?.is_empty() {
                return Err(Invalid("flows with external effect history cannot be retried; inspect retained receipts").into());
            }
        }
        let kind = definition["type"].as_str().unwrap_or("");
        let _guard = if kind == "model" || kind.starts_with("plugin.") {
            Some(self.effect_lock(id)?)
        } else {
            None
        };
        if _guard.is_some() {
            self.store.fence_unstarted(&e)?;
        }
        if self
            .store
            .effect_observation(&e)?
            .is_some_and(|v| v["state"] != "not_applied")
        {
            return Err(Invalid("recorded or uncertain external effect cannot be replayed").into());
        }
        self.run_with_params(
            actor,
            old["work_id"].as_str().ok_or(Invalid("work missing"))?,
            key,
            Some(id),
            e.invocation.source_value()["parameters"]
                .get("input")
                .map(|_| self.invocation_params(&e))
                .transpose()?,
        )
        .await
    }
    pub async fn reconcile(
        &self,
        actor: &str,
        id: &str,
        decision: &str,
        note: &str,
        output: Value,
        settled: bool,
    ) -> Result<Value> {
        self.reconcile_selected(actor, id, decision, note, output, settled, None)
            .await
    }
    // Keep the established reconciliation call shape while adding an optional exact effect selector.
    #[allow(clippy::too_many_arguments)]
    pub async fn reconcile_selected(
        &self,
        actor: &str,
        id: &str,
        decision: &str,
        note: &str,
        output: Value,
        settled: bool,
        effect_id: Option<&str>,
    ) -> Result<Value> {
        let e = self.owned(actor, id)?;
        self.permit(actor, "reconcile", None)?;
        if !settled
            || note.trim().is_empty()
            || note.len() > 4096
            || !["completed", "not_applied"].contains(&decision)
        {
            return Err(Invalid(
                "explicit settled provider evidence and completed/not_applied decision required",
            )
            .into());
        }
        if self.lost_run(id)?.is_some() {
            self.reconcile_lost(actor, id, decision, note, &output)?;
            return self.inspect(actor, id).await;
        }
        let rows = self
            .native()?
            .list_workflows(&dbos::sysdb::types::WorkflowFilter {
                workflow_ids: vec![id],
                load_output: false,
                ..Default::default()
            })
            .await?;
        if !rows.first().is_some_and(|r| {
            [
                "ERROR",
                "CANCELLED",
                "SUCCESS",
                "MAX_RECOVERY_ATTEMPTS_EXCEEDED",
            ]
            .contains(&r.status.as_str())
        }) {
            return Err(
                Invalid("cancel or wait for a terminal workflow before reconciliation").into(),
            );
        }
        if crate::flow_core::is_ledgered(
            &self.work(
                actor,
                e.invocation.source_value()["workspace"]
                    .as_str()
                    .ok_or(Invalid("work missing"))?,
            )?["definition"],
        ) {
            self.reconcile_flow(&e, effect_id, decision, note, output)?;
            return self.inspect(actor, id).await;
        }
        if effect_id.is_some() {
            return Err(Invalid("legacy run has no named effect_id").into());
        }
        let _guard = self.effect_lock(id)?;
        let receipt = if decision == "completed" {
            Some(self.receipt(&e, output)?)
        } else {
            if self.output_path(id).exists() {
                return Err(
                    Invalid("recorded output contradicts not-applied; inspect evidence").into(),
                );
            }
            None
        };
        self.store
            .reconcile_effect(&e, receipt.as_ref(), actor, note)?;
        self.record(actor, "reconcile", id, &format!("{decision}: {note}"))?;
        self.inspect(actor, id).await
    }
    pub fn validate_schedule(&self, s: &dbos::Schedule) -> Result<()> {
        let context = s.decode_context::<ScheduleContext>()?;
        // The retained row is the authorized definition. While an in-place edit is
        // still being applied natively (`pending`), the previous revision is also valid.
        let expected = self.schedule_expectations(s.schedule_name())?;
        if expected.is_empty() {
            return Err(Invalid("schedule lacks an authorized immutable definition").into());
        }
        let timing_ok = expected.iter().any(|(raw, cron, tz)| {
            serde_json::from_str::<ScheduleContext>(raw).is_ok_and(|c| c == context)
                && s.schedule() == cron
                && s.cron_timezone() == Some(tz.as_str())
        });
        if context.home_id != self.policy.home_id
            || !timing_ok
            || s.workflow() != &dbos::sysdb::types::QualifiedWorkflowName::new(SCHEDULED)
            || s.app_name() != Some(self.config.application.as_str())
            || s.queue_name() != Some(self.config.work_queue().as_str())
            || s.automatic_backfill()
        {
            return Err(Invalid("stored schedule differs from admitted scope or timing").into());
        }
        self.work(&context.actor, &context.work_id)?;
        Ok(())
    }
    pub fn validate_scheduled_context(&self, context: &ScheduleContext) -> Result<()> {
        if context.home_id != self.policy.home_id {
            return Err(Invalid("scheduled invocation scope differs").into());
        }
        // The immutable schedule definition is retained after deletion for queued occurrences.
        // Every revision of the schedule was authorized when it was made; an occurrence
        // queued before an in-place edit keeps the revision it was admitted under.
        let retained = self.schedule_expectations(&context.name)?;
        if retained.is_empty() {
            return Err(Invalid("scheduled invocation was never authorized").into());
        }
        let revisions = self.schedule_revision_contexts(&context.name)?;
        let matches = |raw: &String| {
            serde_json::from_str::<ScheduleContext>(raw).is_ok_and(|c| c == *context)
        };
        if !retained.iter().any(|(raw, _, _)| matches(raw)) && !revisions.iter().any(matches) {
            return Err(Invalid("scheduled context mismatch").into());
        }
        self.permit(&context.actor, "schedule", None)?;
        self.work(&context.actor, &context.work_id)?;
        Ok(())
    }
    /// The idempotent admission handoff one due occurrence performs. Admission is
    /// durable before the later fallible audit/submit steps; see `scheduled_admission`.
    pub async fn schedule_handoff(
        &self,
        context: &ScheduleContext,
        due: std::time::SystemTime,
    ) -> Result<Value> {
        self.schedule_handoff_for(context, due, None).await
    }
    /// The handoff of one native firing identified by its workflow id.
    pub async fn schedule_handoff_for(
        &self,
        context: &ScheduleContext,
        due: std::time::SystemTime,
        occurrence_id: Option<&str>,
    ) -> Result<Value> {
        self.validate_scheduled_context(context)?;
        self.run_with_params(
            &context.actor,
            &context.work_id,
            &occurrence_key(&context.name, due, occurrence_id)?,
            None,
            context.params.clone(),
        )
        .await
    }
    /// Read-only: the existing durable admission of the occurrence due at `due`, if
    /// any. Independent of the handoff's own outcome, which can fail after admitting.
    pub fn scheduled_admission(
        &self,
        context: &ScheduleContext,
        due: std::time::SystemTime,
    ) -> Result<Option<String>> {
        self.scheduled_admission_for(context, due, None)
    }
    /// The admission of one native firing identified by its workflow id.
    pub fn scheduled_admission_for(
        &self,
        context: &ScheduleContext,
        due: std::time::SystemTime,
        occurrence_id: Option<&str>,
    ) -> Result<Option<String>> {
        let key = occurrence_key(&context.name, due, occurrence_id)?;
        let request = request_id(&context.work_id, &key)?;
        let id = Invocation::derive_dbos_id(&context.home_id, &context.actor, WORKFLOW, &request);
        let Some(record) = self.store.inspect(&id)? else {
            return Ok(None);
        };
        let iv = &record.envelope.invocation;
        if context.home_id != self.policy.home_id
            || iv.home_id() != context.home_id
            || iv.scope() != context.actor
            || iv.principal() != context.actor
            || iv.workflow_name() != WORKFLOW
            || iv.workspace() != context.work_id
            || iv.request_id() != request
        {
            return Err(Invalid("scheduled admission differs from retained schedule").into());
        }
        Ok(Some(id))
    }
    pub fn validate_scheduled_input(&self, row: &dbos::sysdb::types::WorkflowRecord) -> Result<()> {
        let input: dbos::ScheduledWorkflowInput<ScheduleContext> = serde_json::from_str(
            row.input
                .as_deref()
                .ok_or(Invalid("scheduled input missing"))?,
        )?;
        let c = &input.context;
        if c.home_id != self.policy.home_id
            || row.queue_name.as_deref() != Some(self.config.work_queue().as_str())
            || row.queue_partition_key.is_some()
            || row.parent_workflow_id.is_some()
            || row.forked_from.is_some()
            || row.serialization.as_deref() != Some("rust_serde")
        {
            return Err(Invalid("scheduled invocation scope differs").into());
        }
        self.validate_scheduled_context(c)
    }
    /// The canonical operator projection of one native schedule. Timing, catch-up and
    /// firing are DBOS-native: Forge only previews upcoming ticks for display.
    fn schedule_view(&self, s: &dbos::Schedule, c: &ScheduleContext) -> Result<Value> {
        let next = if s.status().as_str() == "ACTIVE" {
            DbosCron::parse(s.schedule(), s.cron_timezone().unwrap_or("UTC"))?
                .upcoming(1)
                .first()
                .map(|t| t["local"].clone())
        } else {
            None
        };
        let last = s
            .last_fired_at()
            .map(|t| chrono::DateTime::<Utc>::from(t).to_rfc3339());
        let mut view = json!({"schedule_name":s.schedule_name(),"schedule":s.schedule(),"cron_timezone":s.cron_timezone(),"status":s.status().as_str(),"next_run":next,"last_fired_at":last,"automatic_backfill":s.automatic_backfill(),"catchup":CATCHUP,"context":{"actor":c.actor,"work_id":c.work_id,"params":crate::inspection::preview(&json!(c.params))}});
        view["tags"] = json!(self.schedule_tags(s.schedule_name())?);
        view["revision"] = json!(self.schedule_revision(s.schedule_name())?);
        Ok(view)
    }
    pub async fn schedules(&self, actor: &str) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let mut out = Vec::new();
        for s in self.native()?.list_schedules(&Default::default()).await? {
            if s.workflow() != &dbos::sysdb::types::QualifiedWorkflowName::new(SCHEDULED) {
                continue;
            }
            self.validate_schedule(&s)?;
            let c = s.decode_context::<ScheduleContext>()?;
            if c.actor != actor {
                continue;
            }
            out.push(self.schedule_view(&s, &c)?);
        }
        Ok(json!(out))
    }
    /// Validate a cron expression and IANA zone exactly as schedule creation does and
    /// preview its next due ticks, without storing anything.
    pub fn schedule_preview(
        &self,
        actor: &str,
        cron: &str,
        tz: &str,
        count: Option<i64>,
    ) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let count = count.unwrap_or(5);
        if !(1..=20).contains(&count) {
            return Err(Invalid("count must be between 1 and 20").into());
        }
        let (normalized, parsed) = schedule_timing(cron, tz)?;
        let upcoming = parsed.upcoming(count as usize);
        Ok(
            json!({"valid":true,"input":cron,"schedule":normalized,"cron_timezone":tz,"upcoming":upcoming,"catchup":CATCHUP,"automatic_backfill":false,"authority":TIMING_AUTHORITY}),
        )
    }
    /// One owned schedule: its retained immutable intent, native DBOS state (or
    /// `DELETED` once removed natively), upcoming ticks, recent occurrence history
    /// with outcome counts, and the actions currently applicable.
    pub async fn schedule_show(
        &self,
        actor: &str,
        name: &str,
        limit: Option<i64>,
    ) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let history = self.schedule_occurrences(actor, name, limit).await?;
        let (raw, cron_text, tz, pending, revision) = self
            .store
            .write(|c| {
                Ok(c.query_row(
                    "SELECT context,cron,tz,pending,rev FROM forge_schedules WHERE name=?1",
                    [name],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, bool>(3)?,
                            r.get::<_, i64>(4)?,
                        ))
                    },
                )
                .optional()?)
            })?
            .ok_or(Denied("schedule missing or outside scope"))?;
        let retained: ScheduleContext = serde_json::from_str(&raw)?;
        let native = self.native()?.get_schedule(name).await?;
        let (mut view, active_params) = match &native {
            Some(s)
                if s.workflow() == &dbos::sysdb::types::QualifiedWorkflowName::new(SCHEDULED) =>
            {
                self.validate_schedule(s)?;
                let actual = s.decode_context::<ScheduleContext>()?;
                (self.schedule_view(s, &actual)?, actual.params)
            }
            Some(_) => return Err(Denied("schedule missing or outside scope").into()),
            None => {
                let held = json!({"schedule_name":name,"schedule":cron_text,"cron_timezone":tz,"status":"DELETED","next_run":null,"last_fired_at":null,"automatic_backfill":false,"catchup":CATCHUP,"tags":self.schedule_tags(name)?,"revision":revision,"context":{"actor":retained.actor,"work_id":retained.work_id,"params":crate::inspection::preview(&json!(retained.params))}});
                (held, retained.params.clone())
            }
        };
        let status = view["status"].as_str().unwrap_or("").to_owned();
        let mut upcoming = Vec::new();
        if let Some(s) = native.as_ref().filter(|_| status == "ACTIVE") {
            upcoming =
                DbosCron::parse(s.schedule(), s.cron_timezone().unwrap_or("UTC"))?.upcoming(3);
        }
        let mut counts = serde_json::Map::new();
        for o in history["occurrences"].as_array().into_iter().flatten() {
            let key = o["outcome"].as_str().unwrap_or("unknown").to_owned();
            let n = counts.get(&key).and_then(Value::as_u64).unwrap_or(0);
            counts.insert(key, json!(n + 1));
        }
        view["params"] = json!(active_params);
        view["upcoming"] = json!(upcoming);
        view["authority"] = if pending {
            json!(
                "A saved edit is pending. Native timing is shown when available. Inspect the pending edit in Canonical JSON."
            )
        } else {
            json!(TIMING_AUTHORITY)
        };
        view["pending_edit"] = if pending {
            json!({
                "revision": revision,
                "schedule": cron_text,
                "cron_timezone": tz,
                "params": retained.params,
                "native_present": native.is_some(),
            })
        } else {
            Value::Null
        };
        view["actions"] = json!({"pause":status=="ACTIVE","resume":!pending && status=="PAUSED","delete":!pending && status!="DELETED","replace":true,"edit":!pending && status!="DELETED"});
        view["replacement"] = json!(REPLACEMENT);
        view["occurrences"] = history["occurrences"].clone();
        view["outcome_counts"] = Value::Object(counts);
        view["limit"] = history["limit"].clone();
        Ok(view)
    }
    pub async fn schedule(
        &self,
        actor: &str,
        name: &str,
        id: &str,
        cron: &str,
        tz: &str,
    ) -> Result<Value> {
        self.schedule_with_params(actor, name, id, cron, tz, None)
            .await
    }
    pub async fn schedule_with_params(
        &self,
        actor: &str,
        name: &str,
        id: &str,
        cron: &str,
        tz: &str,
        input: Option<Value>,
    ) -> Result<Value> {
        self.schedule_tagged(actor, name, id, cron, tz, input, None)
            .await
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn schedule_tagged(
        &self,
        actor: &str,
        name: &str,
        id: &str,
        cron: &str,
        tz: &str,
        input: Option<Value>,
        tags: Option<Vec<String>>,
    ) -> Result<Value> {
        identifier(name)?;
        let tags = tags
            .map(|t| crate::schedule_edit::normalize_tags(&t))
            .transpose()?;
        self.permit(actor, "schedule", None)?;
        let definition = self.admitted(actor, id)?;
        self.validate_definition(actor, &definition, "execute")?;
        if !crate::flow_core::accepts_params(&definition)
            && input
                .as_ref()
                .is_some_and(|v| !v.is_object() || v.as_object().is_some_and(|o| !o.is_empty()))
        {
            return Err(Invalid("scheduled params require agent, workflow or harness work").into());
        }
        if let Some(input) = &input {
            self.params_reference(actor, input)?;
        }
        let _edit = self.lock_schedule(&format!("{actor}.{name}")).await?;
        let created = self
            .create_schedule(actor, name, id, cron, tz, input)
            .await?;
        if let Some(tags) = tags {
            self.set_schedule_tags(&created, &tags)?;
        }
        self.schedules(actor).await
    }
    /// Persist the retained context and create the native DBOS schedule. Callers have
    /// already checked authority and the work definition; the work need not be enabled.
    /// The caller holds the canonical name's schedule lock.
    /// Returns the canonical actor-qualified schedule name.
    pub(crate) async fn create_schedule(
        &self,
        actor: &str,
        name: &str,
        id: &str,
        cron: &str,
        tz: &str,
        input: Option<Value>,
    ) -> Result<String> {
        let (normalized, _) = schedule_timing(cron, tz)?;
        let cron = normalized.as_str();
        let name = format!("{actor}.{name}");
        self.require_schedule_settled(&name)?;
        if self
            .native()?
            .list_schedules(&Default::default())
            .await?
            .iter()
            .any(|s| s.schedule_name() == name)
        {
            return Err(
                Invalid("schedule exists; pause/delete explicitly before replacement").into(),
            );
        }
        let context = ScheduleContext {
            home_id: self.policy.home_id.clone(),
            actor: actor.into(),
            work_id: id.into(),
            name: name.clone(),
            params: input,
        };
        let raw = serde_json::to_string(&context)?;
        self.store.write(|c| {
            let old = c
                .query_row(
                    "SELECT context,cron,tz FROM forge_schedules WHERE name=?1",
                    [&name],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?;
            if let Some(old) = old {
                if old != (raw.clone(), cron.into(), tz.into()) {
                    return Err(crate::dbos::admission::Error::Invalid(
                        "schedule identity immutable; use a new name for different intent",
                    ));
                }
            } else {
                c.execute(
                    "INSERT INTO forge_schedules(name,context,cron,tz) VALUES(?1,?2,?3,?4)",
                    params![name, raw, cron, tz],
                )?;
            }
            Ok(())
        })?;
        let mut spec = dbos::ScheduleSpec::for_workflow(
            &name,
            dbos::sysdb::types::QualifiedWorkflowName::new(SCHEDULED),
            cron,
            &context,
        )?;
        spec.automatic_backfill = false;
        spec.cron_timezone = Some(tz.into());
        spec.queue_name = Some(self.config.work_queue());
        spec.app_name = Some(self.config.application.clone());
        self.native()?.create_schedule(&spec).await?;
        self.record(actor, "schedule", &name, "")?;
        Ok(name)
    }
    pub async fn schedule_action(&self, actor: &str, name: &str, action: &str) -> Result<Value> {
        self.permit(actor, "schedule", None)?;
        let _edit = self.lock_schedule(name).await?;
        let schedules = self.schedules(actor).await?;
        if !schedules
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["schedule_name"] == name)
        {
            return Err(Denied("schedule missing or outside scope").into());
        }
        // Pausing an owned, validated native schedule does not modify its
        // timing or clear its pending edit. It remains subject to the name lock.
        if action != "pause" {
            self.require_schedule_settled(name)?;
        }
        match action {
            "pause" => self.native()?.pause_schedule(name).await?,
            "resume" => self.native()?.resume_schedule(name).await?,
            "delete" => self.native()?.delete_schedule(name).await?,
            _ => return Err(Invalid("use pause/resume/delete").into()),
        };
        self.record(actor, action, name, "")?;
        self.schedules(actor).await
    }
    /// Explicitly run an owned saved schedule once, now, with its frozen work and
    /// params, through DBOS's native `trigger_schedule`. Each call is a new manual
    /// run under a native id taken from the clock: there is no request-key
    /// idempotency, so callers must not retry automatically. The schedule's cron,
    /// params, status and last-fired tick are not changed (a PAUSED schedule stays
    /// paused). Success is acknowledged only with the actual native trigger id.
    pub async fn schedule_trigger(&self, actor: &str, name: &str) -> Result<Value> {
        self.permit(actor, "schedule", None)?;
        let edit = self.lock_schedule(name).await?;
        let retained = self
            .store
            .write(|c| {
                Ok(c.query_row(
                    "SELECT context FROM forge_schedules WHERE name=?1",
                    [name],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
            })?
            .ok_or(Denied("schedule missing or outside scope"))?;
        let context: ScheduleContext = serde_json::from_str(&retained)?;
        if context.actor != actor || context.home_id != self.policy.home_id || context.name != name
        {
            return Err(Denied("schedule missing or outside scope").into());
        }
        self.require_schedule_settled(name)?;
        // Frozen work must still be runnable by this actor: enabled and permitted.
        self.admitted(actor, &context.work_id)?;
        let native = self.native()?;
        let Some(schedule) = native.get_schedule(name).await? else {
            return Err(Invalid(
                "schedule is deleted; its history stays readable, but it cannot be run",
            )
            .into());
        };
        if schedule.workflow() != &dbos::sysdb::types::QualifiedWorkflowName::new(SCHEDULED) {
            return Err(Denied("schedule missing or outside scope").into());
        }
        self.validate_schedule(&schedule)?;
        let status = schedule.status().as_str().to_owned();
        let handle = native
            .trigger_schedule::<Value, WorkFault>(name)
            .await
            .map_err(|_| {
                Invalid(
                    "The manual run request could not be confirmed. It may or may not have been enqueued. It was not retried. Check `agent-forge schedules history NAME` for a manual occurrence before running again.",
                )
            })?;
        drop(edit);
        let trigger_id = handle.workflow_id().to_owned();
        if !manual_trigger(name, &trigger_id) {
            return Err(Invalid("native trigger returned an unexpected identity").into());
        }
        self.record(actor, "schedule_trigger", name, &trigger_id)?;
        // Bounded readback: the native handoff admits the run shortly after. Never
        // re-trigger; report whatever is actually recorded.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut occurrence = Value::Null;
        loop {
            let history = self.schedule_occurrences(actor, name, Some(20)).await?;
            if let Some(o) = history["occurrences"]
                .as_array()
                .and_then(|l| l.iter().find(|o| o["occurrence_id"] == trigger_id.as_str()))
            {
                occurrence = o.clone();
                if o["run_id"].is_string() || std::time::Instant::now() >= deadline {
                    break;
                }
            } else if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let run_id = occurrence["run_id"].clone();
        let after = native.get_schedule(name).await?;
        Ok(json!({
            "schedule_name": name,
            "kind": "manual",
            "trigger_id": trigger_id,
            "work_id": context.work_id,
            "params": crate::inspection::preview(&json!(context.params)),
            "handoff": occurrence["handoff"].as_str().unwrap_or("ENQUEUED"),
            "run_id": run_id,
            "outcome": occurrence["outcome"].as_str().unwrap_or("handoff_pending"),
            "schedule_status": after.as_ref().map(|s| s.status().as_str().to_owned()).unwrap_or(status),
            "note": if run_id.is_string() {
                "A new manual run of this schedule's frozen work and params. It is not a due tick; the schedule's cron, status and last-fired tick are unchanged. Follow the run for its result."
            } else {
                "The native manual trigger is enqueued, but its run was not admitted yet. Do not run again to hurry it: read `agent-forge schedules history NAME`, or this schedule's history, for this trigger_id."
            },
        }))
    }
    /// Read back recent due occurrences of an owned schedule and the outcome of the run
    /// each one admitted. Derived solely from retained native DBOS firings and Forge's
    /// existing admissions; readable after pause, delete or service restart. The
    /// firing's handoff status is reported separately from the admitted run's status.
    pub async fn schedule_occurrences(
        &self,
        actor: &str,
        name: &str,
        limit: Option<i64>,
    ) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let limit = limit.unwrap_or(10);
        if !(1..=100).contains(&limit) {
            return Err(Invalid("limit must be between 1 and 100").into());
        }
        // The retained immutable definition outlives native pause/delete.
        let retained = self
            .store
            .write(|c| {
                Ok(c.query_row(
                    "SELECT context FROM forge_schedules WHERE name=?1",
                    [name],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
            })?
            .ok_or(Denied("schedule missing or outside scope"))?;
        let context: ScheduleContext = serde_json::from_str(&retained)?;
        if context.actor != actor || context.home_id != self.policy.home_id {
            return Err(Denied("schedule missing or outside scope").into());
        }
        let rows = self
            .native()?
            .list_workflows(&dbos::sysdb::types::WorkflowFilter {
                names: vec![SCHEDULED],
                schedule_names: vec![name],
                load_input: true,
                load_output: true,
                sort_desc: true,
                limit: Some(limit),
                ..Default::default()
            })
            .await?;
        let mut occurrences = Vec::new();
        for row in rows {
            let input: dbos::ScheduledWorkflowInput<ScheduleContext> = serde_json::from_str(
                row.input
                    .as_deref()
                    .ok_or(Invalid("scheduled input missing"))?,
            )?;
            if input.context != context
                || row.queue_name.as_deref() != Some(self.config.work_queue().as_str())
            {
                return Err(Invalid("scheduled occurrence differs from retained schedule").into());
            }
            let scheduled_time = chrono::DateTime::<Utc>::from(input.scheduled_time).to_rfc3339();
            // The run is the durable admission of this occurrence's deterministic
            // identity, whether the handoff succeeded, failed after admitting, or
            // is still in progress. The handoff output only acknowledges it.
            let manual = manual_trigger(name, &row.workflow_id);
            let run_id = self.scheduled_admission_for(
                &context,
                input.scheduled_time,
                Some(&row.workflow_id),
            )?;
            let acknowledged = row
                .output
                .as_deref()
                .and_then(|v| serde_json::from_str::<Value>(v).ok())
                .and_then(|v| v["run_id"].as_str().map(str::to_owned));
            if acknowledged.is_some() && acknowledged != run_id {
                return Err(Invalid("scheduled handoff acknowledges a different run").into());
            }
            let (run, failure) = match &run_id {
                Some(id) => {
                    let full = self.inspect(actor, id).await?;
                    let failure = full["workflow"]["failure"].clone();
                    (crate::inspection::run_summary(full), failure)
                }
                None => (Value::Null, Value::Null),
            };
            let outcome = occurrence_outcome(row.status.as_str(), run["status"].as_str());
            let handoff_error = row.error.as_ref().map(|_| {
                if run_id.is_some() {
                    "Scheduled handoff failed after admission; the admitted run proceeds independently. Its own status is in run."
                } else {
                    "Scheduled handoff failed before admission; the occurrence admitted no run."
                }
            });
            occurrences.push(json!({"occurrence_id":row.workflow_id,"kind":if manual {"manual"} else {"due"},"scheduled_time":scheduled_time,"handoff":row.status.as_str(),"handoff_error":handoff_error,"run_id":run_id,"outcome":outcome,"failure":failure,"run":run}));
        }
        Ok(
            json!({"schedule_name":name,"work_id":context.work_id,"limit":limit,"occurrences":occurrences}),
        )
    }
    pub async fn snapshot(&self, actor: &str) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let ids = self.store.write(|c| {
            let mut s =
                c.prepare("SELECT id FROM forge_work WHERE owner=?1 ORDER BY id LIMIT 100")?;
            Ok(s.query_map([actor], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        let work = ids
            .iter()
            .map(|id| {
                self.work(actor, id).map(|mut w| {
                    w["definition"] = json!({"type":w["definition"]["type"]});
                    w["summary"] = json!(true);
                    w
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let ids = self.store.write(|c| {
            let mut s =
                c.prepare("SELECT dbos_id FROM dbos_invocations WHERE json_extract(envelope,'$.invocation.principal')=?1 ORDER BY rowid DESC LIMIT 100")?;
            Ok(s.query_map([actor], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        let mut runs = Vec::new();
        for id in ids {
            if self
                .store
                .inspect(&id)?
                .is_some_and(|r| r.envelope.invocation.principal() == actor)
            {
                runs.push(crate::inspection::run_summary(
                    self.inspect(actor, &id).await?,
                ));
                if runs.len() == 100 {
                    break;
                }
            }
        }
        Ok(
            json!({"limits":{"work":100,"runs":100},"health":{"database":"ok","engine":"DBOS Rust 0.6.0","scheduler":"running","plugins":self.plugins.keys().collect::<Vec<_>>(),"installed_plugins":self.plugins_view(),"time":Utc::now().to_rfc3339()},"types":self.types(),"models":self.policy.models.keys().collect::<Vec<_>>(),"runners":self.runners(actor).await?,"work":work,"schedules":self.schedules(actor).await?,"runs":runs}),
        )
    }
}
/// A cron expression read in DBOS's own dialect: the same `croner` options, name
/// spellings and `jiff` time-zone database the DBOS scheduler fires with, so weekday
/// numbering (0 or 7 = Sunday), DST handling and validity agree with actual firing.
pub(crate) struct DbosCron {
    cron: croner::Cron,
    zone: jiff::tz::TimeZone,
}
impl DbosCron {
    fn parse(expression: &str, tz: &str) -> Result<Self> {
        let zone = jiff::tz::TimeZone::get(tz).map_err(|_| {
            Invalid(
                "invalid IANA time zone; use a region name such as UTC, Europe/Paris or America/Chicago",
            )
        })?;
        let mut normalized = expression.trim().to_uppercase();
        for (long, short) in [
            ("MONDAY", "MON"),
            ("TUESDAY", "TUE"),
            ("WEDNESDAY", "WED"),
            ("THURSDAY", "THU"),
            ("FRIDAY", "FRI"),
            ("SATURDAY", "SAT"),
            ("SUNDAY", "SUN"),
            ("JANUARY", "JAN"),
            ("FEBRUARY", "FEB"),
            ("MARCH", "MAR"),
            ("APRIL", "APR"),
            ("JUNE", "JUN"),
            ("JULY", "JUL"),
            ("AUGUST", "AUG"),
            ("SEPTEMBER", "SEP"),
            ("OCTOBER", "OCT"),
            ("NOVEMBER", "NOV"),
            ("DECEMBER", "DEC"),
        ] {
            normalized = normalized.replace(long, short);
        }
        let cron = croner::parser::CronParser::builder()
            .seconds(croner::parser::Seconds::Optional)
            .year(croner::parser::Year::Disallowed)
            .dom_and_dow(true)
            .build()
            .parse(&normalized)
            .map_err(|_| Invalid(CRON_HELP))?;
        let parsed = Self { cron, zone };
        if parsed.upcoming(1).is_empty() {
            return Err(Invalid("invalid cron: the pattern never fires").into());
        }
        Ok(parsed)
    }
    /// The next `count` ticks strictly after now, on the zone's wall clock and in UTC.
    fn upcoming(&self, count: usize) -> Vec<Value> {
        let mut out = Vec::new();
        let mut after = jiff::Timestamp::now().to_zoned(self.zone.clone());
        while out.len() < count {
            let Ok(next) = self.cron.find_next_occurrence(&after, false) else {
                break;
            };
            out.push(json!({
                "local": next.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string(),
                "utc": next.with_time_zone(jiff::tz::TimeZone::UTC).strftime("%Y-%m-%dT%H:%M:%S%:z").to_string(),
            }));
            after = next;
        }
        out
    }
}
const CRON_HELP: &str = "invalid cron: use five fields (minute hour day month weekday) or six with seconds first; weekday 0 or 7 is Sunday";
/// Shared creation/preview timing check: explicit IANA zone and five-field (minute
/// first, second 0) or six-field (second first) cron, valid in the DBOS dialect.
/// Returns the stored six-field form.
/// Validate a cron expression and IANA zone exactly as schedule creation does.
pub(crate) fn check_schedule_timing(cron: &str, tz: &str) -> Result<()> {
    schedule_timing(cron, tz).map(|_| ())
}
pub(crate) fn schedule_timing(cron: &str, tz: &str) -> Result<(String, DbosCron)> {
    let normalized = if cron.split_whitespace().count() == 5 {
        format!("0 {}", cron.trim())
    } else {
        cron.trim().to_owned()
    };
    let parsed = DbosCron::parse(&normalized, tz)?;
    if normalized.split_whitespace().count() != 6 {
        return Err(Invalid(CRON_HELP).into());
    }
    Ok((normalized, parsed))
}
/// Native DBOS catch-up policy for every Forge user schedule (automatic_backfill=false).
pub const CATCHUP: &str = "skip: ticks missed while paused or while no service runs are not admitted; resuming fires from the next tick";
pub const TIMING_AUTHORITY: &str = "DBOS fires ticks on the schedule's IANA wall clock (DST-aware); Forge's upcoming times are a display preview";
pub const REPLACEMENT: &str = "A schedule's work is fixed once admitted; to change the work create a replacement under a new name. Cron, zone, params, enabled and tags are edited in place";
/// Logical outcome of one due occurrence, distinct from the native handoff status.
pub fn occurrence_outcome(handoff: &str, run_status: Option<&str>) -> &'static str {
    match run_status {
        Some("SUCCESS") => "succeeded",
        Some("ERROR" | "MAX_RECOVERY_ATTEMPTS_EXCEEDED" | "RECOVERY_REQUIRED") => "failed",
        Some("CANCELLED") => "cancelled",
        Some("ADMITTED" | "PENDING" | "ENQUEUED" | "DELAYED") => "running",
        Some(_) => "unknown",
        None => match handoff {
            "PENDING" | "ENQUEUED" | "DELAYED" => "handoff_pending",
            _ => "not_admitted",
        },
    }
}

#[cfg(test)]
mod schedule_timing_tests {
    use super::*;
    #[test]
    fn manual_triggers_are_identified_by_exact_name_and_keyed_per_trigger() {
        let due = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
        let name = "operator.digest";
        let t1 = "sched-operator.digest-trigger-2026-11-16T21:00:00.123456789Z";
        let t2 = "sched-operator.digest-trigger-2026-11-16T21:00:00.123456790Z";
        assert!(manual_trigger(name, t1));
        // A due tick and a different schedule whose name extends this one are not triggers.
        assert!(!manual_trigger(
            name,
            "sched-operator.digest-2026-11-16T21:00:00Z"
        ));
        assert!(!manual_trigger("operator.dig", t1));
        assert!(!manual_trigger(
            "operator.digest",
            "sched-operator.digest-v2-trigger-2026-11-16T21:00:00Z"
        ));
        // Two triggers are two keys, even at the same millisecond; a due tick keeps its key.
        let k1 = occurrence_key(name, due, Some(t1)).unwrap();
        assert_ne!(k1, occurrence_key(name, due, Some(t2)).unwrap());
        assert_eq!(k1, occurrence_key(name, due, Some(t1)).unwrap());
        assert_ne!(k1, schedule_key(name, due).unwrap());
        assert_eq!(
            occurrence_key(
                name,
                due,
                Some("sched-operator.digest-2026-11-16T21:00:00Z")
            )
            .unwrap(),
            schedule_key(name, due).unwrap()
        );
        assert_eq!(
            occurrence_key(name, due, None).unwrap(),
            schedule_key(name, due).unwrap()
        );
    }
    #[test]
    fn timing_uses_the_dbos_dialect_weekdays_zones_and_validity() {
        // 1-5 is Monday-Friday in DBOS's dialect (the `cron` crate would read Sunday-Thursday).
        let (stored, parsed) = schedule_timing("0 9 * * 1-5", "America/Chicago").unwrap();
        assert_eq!(stored, "0 0 9 * * 1-5");
        for tick in parsed.upcoming(10) {
            let local: jiff::Zoned =
                format!("{}[America/Chicago]", tick["local"].as_str().unwrap())
                    .parse()
                    .unwrap();
            assert!(!matches!(
                local.weekday(),
                jiff::civil::Weekday::Saturday | jiff::civil::Weekday::Sunday
            ));
            assert_eq!(local.hour(), 9);
        }
        // Sunday as 0 is accepted, as DBOS accepts it.
        assert!(schedule_timing("0 0 9 * * 0", "UTC").is_ok());
        let error = |c: &str, z: &str| schedule_timing(c, z).err().unwrap().to_string();
        assert!(error("*/20 * * * * *", "Mars/Olympus").contains("invalid IANA time zone"));
        assert!(error("61 * * * *", "UTC").contains("invalid cron"));
        assert!(error("0 0 31 2 *", "UTC").contains("never fires"));
        assert!(error("@daily", "UTC").contains("invalid cron"));
    }
}

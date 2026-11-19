//! Strict plugin manifest (`manifest.json`) in the forge-reference vocabulary.
//!
//! Reference: forge-reference `src/forge/plugin_runtime/manifest.py` (PluginManifest,
//! V2ResourceDeclaration, forbidden metadata keys) and `descriptors.py` (the
//! dashboard, api, cli and mcp descriptors). Parsing is strict: unknown fields,
//! unknown contribution kinds, executable-looking metadata keys, undeclared or
//! unreachable resources and malformed digests are all refused. Nothing here
//! imports, executes or evaluates plugin content.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const MANIFEST_FILE: &str = "manifest.json";
/// Manifest `api_version` of a plugin that speaks stdio protocol version 2.
pub const API_VERSION: &str = "v2";
/// Stdio protocol version an installed plugin must answer with.
pub const PROTOCOL: u64 = 2;
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_RESOURCES: usize = 256;
const MAX_CONTRIBUTIONS: usize = 256;
const MAX_TIMEOUT_SECONDS: u64 = 300;
const MAX_API_BODY: u64 = 1024 * 1024;
const DEFAULT_API_BODY: u64 = 64 * 1024;
/// Metadata keys that would smuggle an import target, hook or secret.
const FORBIDDEN_KEYS: &[&str] = &[
    "import",
    "import_target",
    "module",
    "entrypoint",
    "entry_point",
    "package",
    "hook",
    "callable",
    "secret",
];

#[derive(Debug, thiserror::Error)]
#[error("invalid plugin manifest: {0}")]
pub struct ManifestError(pub String);
fn refuse<T>(message: impl Into<String>) -> Result<T> {
    Err(ManifestError(message.into()).into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Job,
    Workflow,
    Cli,
    Api,
    Mcp,
    Dashboard,
    Assets,
}
impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Job => "job",
            Kind::Workflow => "workflow",
            Kind::Cli => "cli",
            Kind::Api => "api",
            Kind::Mcp => "mcp",
            Kind::Dashboard => "dashboard",
            Kind::Assets => "assets",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceKind {
    Script,
    Job,
    Workflow,
    Asset,
}
impl ResourceKind {
    pub fn name(self) -> &'static str {
        match self {
            ResourceKind::Script => "script",
            ResourceKind::Job => "job",
            ResourceKind::Workflow => "workflow",
            ResourceKind::Asset => "asset",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Resource {
    pub kind: ResourceKind,
    pub id: String,
    /// Relative path inside the release; `sha256:<hex>` of the exact bytes.
    pub path: String,
    pub digest: String,
}
impl Resource {
    pub fn identity(&self) -> String {
        format!("{}:{}", self.kind.name(), self.id)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    /// `script:<id>` of the declared executable resource speaking stdio protocol 2.
    pub executable: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}
fn default_timeout() -> u64 {
    30
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Contribution {
    pub kind: Kind,
    pub id: String,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub version: String,
    pub api_version: String,
    pub description: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<String>,
    pub runtime: Runtime,
    #[serde(default)]
    pub contributions: Vec<Contribution>,
    #[serde(default)]
    pub resource_declarations: Vec<Resource>,
}

// ---- Typed contribution metadata ---------------------------------------------
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DisabledDenial {
    #[serde(default = "denial_status")]
    pub status: u8,
    #[serde(default = "denial_message")]
    pub message: String,
}
fn denial_status() -> u8 {
    3
}
fn denial_message() -> String {
    "plugin is installed but disabled".into()
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CliMeta {
    pub command_path: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub disabled_denial: Option<DisabledDenial>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiRoute {
    pub method: String,
    pub path: String,
    /// `read` or `execute`: the actor capability required (default `execute`).
    #[serde(default = "execute_capability")]
    pub capability: String,
}
fn execute_capability() -> String {
    "execute".into()
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiMeta {
    pub routes: Vec<ApiRoute>,
    #[serde(default = "default_api_body")]
    pub max_body_bytes: u64,
}
fn default_api_body() -> u64 {
    DEFAULT_API_BODY
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpMeta {
    pub tool_name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub read_only: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardRoute {
    pub path: String,
    pub label: String,
    /// `asset:<id>` ES module loaded with `import()`.
    pub entry: String,
    #[serde(default)]
    pub stylesheet: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardMeta {
    pub routes: Vec<DashboardRoute>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetsMeta {
    pub resources: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WorkMeta {
    #[serde(default)]
    pub description: String,
}

/// A job or workflow resource file: work to materialize, with an optional native schedule.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkResource {
    #[serde(default)]
    pub description: String,
    pub work: Value,
    #[serde(default)]
    pub schedule: Option<WorkSchedule>,
    /// Materialized enabled state; absent means disabled.
    #[serde(default)]
    pub enabled: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSchedule {
    pub cron: String,
    pub timezone: String,
}

pub fn parse_work_resource(bytes: &[u8]) -> Result<WorkResource> {
    let resource: WorkResource = match serde_json::from_slice(bytes) {
        Ok(r) => r,
        Err(_) => return refuse("job/workflow resource is not valid strict JSON"),
    };
    let work = resource.work.as_object();
    if work.is_none_or(|w| w.len() != 2 || !w.contains_key("type") || !w.contains_key("input")) {
        return refuse("resource work must contain exactly type and input");
    }
    Ok(resource)
}

// ---- Validators ---------------------------------------------------------------
pub fn plugin_id(value: &str) -> bool {
    // ^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$, at most 48 characters.
    value.len() <= 48
        && segments(value, '-')
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
}
/// Stable lowercase contribution identifier; no `/`, so it is safe inside a work id.
pub fn contribution_id(value: &str) -> bool {
    value.len() <= 64
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value.split(['.', '_', '-']).all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}
fn segments(value: &str, separator: char) -> bool {
    !value.is_empty()
        && value.split(separator).all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}
fn underscored(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_lowercase) && segments(value, '_')
}
pub fn digest_text(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
fn digest_ok(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
pub fn safe_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.contains(['\\', '\0', '%', '?', '#'])
        && !value.starts_with('/')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}
fn semver(value: &str) -> bool {
    let (rest, build) = match value.split_once('+') {
        Some((r, b)) => (r, Some(b)),
        None => (value, None),
    };
    let (core, pre) = match rest.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (rest, None),
    };
    let numeric = |p: &str| {
        !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && (p == "0" || !p.starts_with('0'))
    };
    let ident =
        |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let core_parts: Vec<_> = core.split('.').collect();
    core_parts.len() == 3
        && core_parts.iter().all(|p| numeric(p))
        && pre.is_none_or(|p| p.split('.').all(&ident))
        && build.is_none_or(|b| b.split('.').all(&ident))
}
fn forbidden(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(k, v)| {
            FORBIDDEN_KEYS.contains(&k.to_ascii_lowercase().as_str()) || forbidden(v)
        }),
        Value::Array(items) => items.iter().any(forbidden),
        _ => false,
    }
}
fn meta<T: for<'de> Deserialize<'de>>(contribution: &Contribution) -> Result<T> {
    let value = if contribution.metadata.is_null() {
        Value::Object(Default::default())
    } else {
        contribution.metadata.clone()
    };
    match serde_json::from_value(value) {
        Ok(v) => Ok(v),
        Err(_) => refuse(format!(
            "{} contribution {} has malformed or unknown metadata",
            contribution.kind.name(),
            contribution.id
        )),
    }
}

impl Manifest {
    /// Parse and validate the manifest bytes. Pure: touches no filesystem.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return refuse("manifest exceeds size limit");
        }
        // Unknown contribution kinds are rejected by the Kind enum, unknown fields by
        // deny_unknown_fields; both surface as one value-free diagnostic.
        let manifest: Manifest = match serde_json::from_slice(bytes) {
            Ok(m) => m,
            Err(error) => {
                return refuse(format!(
                    "manifest is not valid strict JSON (unknown field, unknown contribution kind or wrong type): {}",
                    error.classify_label()
                ));
            }
        };
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        if !plugin_id(&self.id) {
            return refuse("id must be lowercase hyphenated (for example release-notes)");
        }
        if !semver(&self.version) {
            return refuse("version must be a semantic version");
        }
        if self.api_version != API_VERSION {
            return refuse("api_version must be \"v2\" (stdio protocol 2)");
        }
        if self.description.trim().is_empty() || self.description.len() > 2048 {
            return refuse("description must be 1-2048 characters");
        }
        if self.capabilities.iter().any(|c| c.trim().is_empty())
            || unique(&self.capabilities).is_err()
        {
            return refuse("capabilities must be unique non-empty strings");
        }
        for list in [&self.requires, &self.conflicts] {
            if unique(list).is_err() || list.iter().any(|p| !plugin_id(p)) {
                return refuse("requires/conflicts must be unique plugin ids");
            }
            if list.contains(&self.id) {
                return refuse("a plugin cannot require or conflict with itself");
            }
        }
        if self.requires.iter().any(|r| self.conflicts.contains(r)) {
            return refuse("a plugin cannot both require and conflict with the same plugin");
        }
        if !(1..=MAX_TIMEOUT_SECONDS).contains(&self.runtime.timeout_seconds) {
            return refuse("runtime.timeout_seconds must be 1-300");
        }
        if self.resource_declarations.len() > MAX_RESOURCES
            || self.contributions.len() > MAX_CONTRIBUTIONS
        {
            return refuse("too many resources or contributions");
        }
        let mut identities = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for resource in &self.resource_declarations {
            if !contribution_id(&resource.id) {
                return refuse("resource ids must be stable lowercase identifiers");
            }
            if !digest_ok(&resource.digest) {
                return refuse(format!(
                    "resource {} digest must be a lowercase sha256:<64 hex>",
                    resource.identity()
                ));
            }
            if !safe_relative_path(&resource.path) || resource.path == MANIFEST_FILE {
                return refuse(format!(
                    "resource {} path must be a safe relative path",
                    resource.identity()
                ));
            }
            if !identities.insert(resource.identity()) {
                return refuse("resource declarations must be unique");
            }
            if !paths.insert(resource.path.clone()) {
                return refuse("two resources cannot declare the same path");
            }
        }
        let script = self.resource(&self.runtime.executable)?;
        if script.kind != ResourceKind::Script {
            return refuse("runtime.executable must reference a script resource");
        }
        let mut owned = BTreeSet::new();
        let mut work_ids = BTreeSet::new();
        let mut asset_exposed = BTreeSet::new();
        let mut referenced_assets = BTreeSet::new();
        for contribution in &self.contributions {
            if !contribution_id(&contribution.id) {
                return refuse("contribution ids must be stable lowercase identifiers");
            }
            if forbidden(&contribution.metadata) {
                return refuse("metadata contains a forbidden executable/package/secret key");
            }
            if !owned.insert((contribution.kind, contribution.id.clone())) {
                return refuse(format!(
                    "ownership collision: duplicate {} contribution {}",
                    contribution.kind.name(),
                    contribution.id
                ));
            }
            match contribution.kind {
                Kind::Job | Kind::Workflow => {
                    let _: WorkMeta = meta(contribution)?;
                    let want = if contribution.kind == Kind::Job {
                        ResourceKind::Job
                    } else {
                        ResourceKind::Workflow
                    };
                    let key = format!("{}:{}", want.name(), contribution.id);
                    if !identities.contains(&key) {
                        return refuse(format!(
                            "{} contribution has no declared resource {key}",
                            contribution.kind.name()
                        ));
                    }
                    if !work_ids.insert(contribution.id.clone()) {
                        return refuse(format!(
                            "ownership collision: job and workflow share the work name {}",
                            contribution.id
                        ));
                    }
                }
                Kind::Cli => {
                    let m: CliMeta = meta(contribution)?;
                    if m.command_path.is_empty()
                        || m.command_path.len() > 4
                        || m.command_path.iter().any(|p| !contribution_id(p))
                    {
                        return refuse("cli command_path must be 1-4 stable lowercase names");
                    }
                    if m.disabled_denial.as_ref().is_some_and(|d| {
                        d.status == 0
                            || d.message.len() > 256
                            || d.message.contains(['\n', '\r', '\0'])
                    }) {
                        return refuse(
                            "cli disabled_denial needs a nonzero status and a one-line message",
                        );
                    }
                }
                Kind::Api => {
                    let m: ApiMeta = meta(contribution)?;
                    if m.routes.is_empty() || m.routes.len() > 64 {
                        return refuse("api contribution needs 1-64 routes");
                    }
                    if m.max_body_bytes > MAX_API_BODY {
                        return refuse("api max_body_bytes exceeds 1 MiB");
                    }
                    let mut seen = BTreeSet::new();
                    for route in &m.routes {
                        if !["GET", "POST", "PUT", "PATCH", "DELETE"]
                            .contains(&route.method.as_str())
                            || !api_path(&route.path)
                            || !["read", "execute"].contains(&route.capability.as_str())
                            || !seen.insert((route.method.clone(), route.path.clone()))
                        {
                            return refuse(
                                "api routes need a unique allowed method, canonical path and read/execute capability",
                            );
                        }
                    }
                }
                Kind::Mcp => {
                    let m: McpMeta = meta(contribution)?;
                    if !underscored(&m.tool_name)
                        || m.tool_name.contains("__")
                        || m.tool_name.len() > 40
                        || self.id.len() + m.tool_name.len() + 9 > 64
                    {
                        return refuse("mcp tool_name must be lowercase_with_underscores");
                    }
                    if m.description.is_empty() || m.description.len() > 1024 {
                        return refuse("mcp description must be 1-1024 characters");
                    }
                    if !m.input_schema.is_object()
                        || jsonschema::validator_for(&m.input_schema).is_err()
                        || m.input_schema["type"] != "object"
                    {
                        return refuse("mcp input_schema must be a valid object JSON schema");
                    }
                }
                Kind::Dashboard => {
                    let m: DashboardMeta = meta(contribution)?;
                    if m.routes.is_empty() || m.routes.len() > 16 {
                        return refuse("dashboard contribution needs 1-16 routes");
                    }
                    let mut seen = BTreeSet::new();
                    for route in &m.routes {
                        if !(route
                            .path
                            .as_bytes()
                            .first()
                            .is_some_and(u8::is_ascii_lowercase)
                            && segments(&route.path, '-'))
                            || route.label.trim().is_empty()
                            || route.label.chars().count() > 40
                            || route.label.chars().any(char::is_control)
                            || !seen.insert(route.path.clone())
                        {
                            return refuse(
                                "dashboard routes need unique lowercase paths and short labels",
                            );
                        }
                        for reference in
                            std::iter::once(&route.entry).chain(route.stylesheet.as_ref())
                        {
                            referenced_assets.insert(reference.clone());
                        }
                    }
                }
                Kind::Assets => {
                    let m: AssetsMeta = meta(contribution)?;
                    if m.resources.is_empty() {
                        return refuse("assets contribution must list resources");
                    }
                    for reference in &m.resources {
                        if !reference.starts_with("asset:")
                            || self.resource(reference).is_err()
                            || !asset_exposed.insert(reference.clone())
                        {
                            return refuse(
                                "assets contribution must list distinct declared asset: resources",
                            );
                        }
                    }
                }
            }
        }
        for reference in &referenced_assets {
            if !asset_exposed.contains(reference) {
                return refuse(format!(
                    "dashboard references {reference}, which no assets contribution exposes"
                ));
            }
        }
        // Every declared resource must be used: no undeclared (or unreachable) bytes ship.
        for resource in &self.resource_declarations {
            let id = resource.identity();
            let used = match resource.kind {
                ResourceKind::Script => id == self.runtime.executable,
                ResourceKind::Job => self
                    .contributions
                    .iter()
                    .any(|c| c.kind == Kind::Job && c.id == resource.id),
                ResourceKind::Workflow => self
                    .contributions
                    .iter()
                    .any(|c| c.kind == Kind::Workflow && c.id == resource.id),
                ResourceKind::Asset => asset_exposed.contains(&id),
            };
            if !used {
                return refuse(format!(
                    "resource {id} is declared but not used by any contribution"
                ));
            }
        }
        Ok(())
    }

    pub fn resource(&self, identity: &str) -> Result<&Resource> {
        match self
            .resource_declarations
            .iter()
            .find(|r| r.identity() == identity)
        {
            Some(r) => Ok(r),
            None => refuse(format!("undeclared resource {identity}")),
        }
    }
    pub fn executable(&self) -> &Resource {
        self.resource(&self.runtime.executable)
            .expect("validated manifest declares its executable")
    }
    /// Plugin work type name, the same `plugin.NAME` namespace as configured plugins.
    pub fn work_type(&self) -> String {
        format!("plugin.{}", self.id)
    }
    /// Namespaced work id a job or workflow contribution materializes as.
    pub fn work_id(&self, contribution_id: &str) -> String {
        format!("plugin.{}.{}", self.id, contribution_id)
    }
    pub fn of_kind(&self, kind: Kind) -> impl Iterator<Item = &Contribution> {
        self.contributions.iter().filter(move |c| c.kind == kind)
    }
    pub fn cli(&self) -> Vec<CliMeta> {
        self.of_kind(Kind::Cli)
            .filter_map(|c| meta(c).ok())
            .collect()
    }
    pub fn api(&self) -> Vec<ApiMeta> {
        self.of_kind(Kind::Api)
            .filter_map(|c| meta(c).ok())
            .collect()
    }
    pub fn mcp(&self) -> Vec<McpMeta> {
        self.of_kind(Kind::Mcp)
            .filter_map(|c| meta(c).ok())
            .collect()
    }
    pub fn dashboard(&self) -> Vec<DashboardRoute> {
        self.of_kind(Kind::Dashboard)
            .filter_map(|c| meta::<DashboardMeta>(c).ok())
            .flat_map(|m| m.routes)
            .collect()
    }
    /// Public MCP tool name: `plugin_<id>__<tool>` (the double underscore cannot occur
    /// in either part, so names are unambiguous and never collide with `forge_*`).
    pub fn mcp_name(&self, tool: &str) -> String {
        format!("plugin_{}__{}", self.id, tool)
    }
    /// Asset resources an enabled plugin may serve, keyed by their relative path.
    pub fn served_asset(&self, path: &str) -> Option<&Resource> {
        let exposed: BTreeSet<String> = self
            .of_kind(Kind::Assets)
            .filter_map(|c| meta::<AssetsMeta>(c).ok())
            .flat_map(|m| m.resources)
            .collect();
        self.resource_declarations.iter().find(|r| {
            r.kind == ResourceKind::Asset && r.path == path && exposed.contains(&r.identity())
        })
    }
    /// Namespaced surface identities, used for ownership-collision checks.
    pub fn ownership(&self) -> BTreeSet<String> {
        let mut keys = BTreeSet::new();
        keys.insert(format!("plugin:{}", self.id));
        keys.insert(format!("type:{}", self.work_type()));
        for c in &self.contributions {
            match c.kind {
                Kind::Job | Kind::Workflow => {
                    keys.insert(format!("work:{}", self.work_id(&c.id)));
                }
                Kind::Mcp => {
                    if let Ok(m) = meta::<McpMeta>(c) {
                        keys.insert(format!("mcp:{}", self.mcp_name(&m.tool_name)));
                    }
                }
                _ => {}
            }
        }
        keys
    }
}

fn api_path(value: &str) -> bool {
    value.starts_with('/')
        && value.len() <= 128
        && value[1..].split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        })
        || value == "/"
}
fn unique(list: &[String]) -> Result<()> {
    let set: BTreeSet<_> = list.iter().collect();
    if set.len() != list.len() {
        bail!("duplicates");
    }
    Ok(())
}
trait Classify {
    fn classify_label(&self) -> &'static str;
}
impl Classify for serde_json::Error {
    fn classify_label(&self) -> &'static str {
        match self.classify() {
            serde_json::error::Category::Syntax | serde_json::error::Category::Eof => "syntax",
            serde_json::error::Category::Data => "data",
            serde_json::error::Category::Io => "io",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn digest(seed: &str) -> String {
        digest_text(seed.as_bytes())
    }
    fn valid() -> Value {
        json!({
            "id":"demo","version":"1.2.3","api_version":"v2","description":"demo",
            "runtime":{"executable":"script:main"},
            "contributions":[
                {"kind":"job","id":"daily"},
                {"kind":"workflow","id":"flow"},
                {"kind":"cli","id":"hello","metadata":{"command_path":["hello"]}},
                {"kind":"api","id":"status","metadata":{"routes":[{"method":"GET","path":"/status"}]}},
                {"kind":"mcp","id":"echo","metadata":{"tool_name":"echo","description":"d","input_schema":{"type":"object"}}},
                {"kind":"dashboard","id":"page","metadata":{"routes":[{"path":"home","label":"Home","entry":"asset:page-js"}]}},
                {"kind":"assets","id":"static","metadata":{"resources":["asset:page-js"]}}
            ],
            "resource_declarations":[
                {"kind":"script","id":"main","path":"bin/main","digest":digest("a")},
                {"kind":"job","id":"daily","path":"jobs/daily.json","digest":digest("b")},
                {"kind":"workflow","id":"flow","path":"flows/flow.json","digest":digest("c")},
                {"kind":"asset","id":"page-js","path":"ui/page.js","digest":digest("d")}
            ]
        })
    }
    fn parse(value: &Value) -> Result<Manifest> {
        Manifest::parse(&serde_json::to_vec(value).unwrap())
    }
    fn message(value: &Value) -> String {
        parse(value).unwrap_err().to_string()
    }

    #[test]
    fn valid_manifest_with_every_kind_parses() {
        let m = parse(&valid()).unwrap();
        assert_eq!(m.work_id("daily"), "plugin.demo.daily");
        assert_eq!(m.mcp_name("echo"), "plugin_demo__echo");
        assert_eq!(m.dashboard().len(), 1);
        assert!(m.served_asset("ui/page.js").is_some());
        assert!(m.served_asset("bin/main").is_none());
    }

    #[test]
    fn rejects_bad_digests_unknown_kinds_unknown_fields_and_forbidden_keys() {
        let mut v = valid();
        v["resource_declarations"][0]["digest"] = json!("sha256:XYZ");
        assert!(message(&v).contains("digest"));
        let mut v = valid();
        v["resource_declarations"][0]["digest"] = json!(format!("sha256:{}", "A".repeat(64)));
        assert!(message(&v).contains("digest"));
        let mut v = valid();
        v["contributions"][0]["kind"] = json!("agents");
        assert!(message(&v).contains("unknown contribution kind"));
        let mut v = valid();
        v["extra"] = json!(1);
        assert!(parse(&v).is_err());
        for key in [
            "import",
            "Module",
            "entrypoint",
            "hook",
            "callable",
            "secret",
            "package",
        ] {
            let mut v = valid();
            v["contributions"][2]["metadata"]["description"] = json!({ key: "x" });
            assert!(message(&v).contains("forbidden"), "{key}");
        }
    }

    #[test]
    fn rejects_collisions_undeclared_and_unused_resources_and_bad_paths() {
        let mut v = valid();
        v["contributions"]
            .as_array_mut()
            .unwrap()
            .push(json!({"kind":"cli","id":"hello"}));
        assert!(message(&v).contains("ownership collision"));
        let mut v = valid();
        v["contributions"]
            .as_array_mut()
            .unwrap()
            .push(json!({"kind":"workflow","id":"daily"}));
        assert!(message(&v).contains("no declared resource") || message(&v).contains("collision"));
        let mut v = valid();
        v["contributions"][5]["metadata"]["routes"][0]["entry"] = json!("asset:missing");
        assert!(message(&v).contains("assets contribution"));
        let mut v = valid();
        v["resource_declarations"]
            .as_array_mut()
            .unwrap()
            .push(json!({"kind":"asset","id":"extra","path":"x.js","digest":digest("e")}));
        assert!(message(&v).contains("not used"));
        for path in ["../x", "/abs", "a//b", "a/./b", "manifest.json", ""] {
            let mut v = valid();
            v["resource_declarations"][3]["path"] = json!(path);
            assert!(parse(&v).is_err(), "{path}");
        }
        let mut v = valid();
        v["runtime"]["executable"] = json!("asset:page-js");
        assert!(message(&v).contains("script"));
        let mut v = valid();
        v["runtime"]["executable"] = json!("script:ghost");
        assert!(message(&v).contains("undeclared"));
    }

    #[test]
    fn rejects_malformed_identity_version_and_relationships() {
        for (key, value) in [
            ("id", json!("Bad_Id")),
            ("id", json!("trailing-")),
            ("version", json!("1.2")),
            ("version", json!("01.2.3")),
            ("api_version", json!("v1")),
            ("description", json!(" ")),
            ("requires", json!(["demo"])),
            ("conflicts", json!(["demo"])),
            ("requires", json!(["a", "a"])),
        ] {
            let mut v = valid();
            v[key] = value;
            assert!(parse(&v).is_err(), "{key}");
        }
        let mut v = valid();
        v["requires"] = json!(["other"]);
        v["conflicts"] = json!(["other"]);
        assert!(parse(&v).is_err());
        let mut v = valid();
        v["version"] = json!("1.0.0-rc.1+build.5");
        assert!(parse(&v).is_ok());
    }

    #[test]
    fn rejects_bad_surface_metadata() {
        let mut v = valid();
        v["contributions"][3]["metadata"]["routes"][0]["method"] = json!("TRACE");
        assert!(parse(&v).is_err());
        let mut v = valid();
        v["contributions"][3]["metadata"]["routes"][0]["path"] = json!("/a/../b");
        assert!(parse(&v).is_err());
        let mut v = valid();
        v["contributions"][4]["metadata"]["tool_name"] = json!("Bad-Name");
        assert!(parse(&v).is_err());
        let mut v = valid();
        v["contributions"][4]["metadata"]["input_schema"] = json!({"type":"string"});
        assert!(parse(&v).is_err());
        let mut v = valid();
        v["contributions"][2]["metadata"]["command_path"] = json!([]);
        assert!(parse(&v).is_err());
        let mut v = valid();
        v["contributions"][5]["metadata"]["routes"][0]["label"] = json!("x".repeat(41));
        assert!(parse(&v).is_err());
    }

    #[test]
    fn work_resource_is_strict() {
        let ok = br#"{"work":{"type":"echo","input":{"text":"x"}},"schedule":{"cron":"* * * * *","timezone":"UTC"},"enabled":false}"#;
        assert!(parse_work_resource(ok).is_ok());
        assert!(parse_work_resource(br#"{"work":{"type":"echo","input":{}},"retry":3}"#).is_err());
        assert!(parse_work_resource(br#"{"work":{"type":"echo"}}"#).is_err());
        assert!(parse_work_resource(br#"{"work":{"type":"echo","input":{},"x":1}}"#).is_err());
    }
}

//! The single resolver for every runtime location and connection setting.
//!
//! Precedence is always: command-line flag, then environment variable, then the
//! `config.json` section, then the default defined here. No other module may
//! construct a Forge runtime path, PostgreSQL URL, listener address, shell or
//! cgroup root: each asks the [`Settings`] resolved at startup.
//!
//! Secrets never enter diagnostics: errors here name a setting, never its value.
use crate::core::Policy;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, Ipv4Addr},
    path::{Component, Path, PathBuf},
    sync::RwLock,
    time::Duration,
};

/// A refused configuration, safe to print verbatim (it never contains a secret).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);
pub(crate) fn refuse<T>(message: impl Into<String>) -> Result<T> {
    Err(ConfigError(message.into()).into())
}
/// A `config.json` parse failure as a value-free message: unknown and missing key names
/// are shown, but a serde message that would quote a configured value never is.
pub fn config_parse_error(error: serde_json::Error) -> ConfigError {
    use serde_json::error::Category;
    let at = format!("line {} column {}", error.line(), error.column());
    let text = error.to_string();
    let named = text.starts_with("unknown field") || text.starts_with("missing field");
    ConfigError(match error.classify() {
        Category::Data if named => {
            let reason = text.split(" at line ").next().unwrap_or(&text);
            format!("config.json refused: {reason} (at {at})")
        }
        Category::Data => format!("config.json has a value of the wrong type or range at {at}"),
        _ => format!("config.json is not valid JSON ({at})"),
    })
}

// ---- Environment variable names -------------------------------------------
pub const ENV_HOME: &str = "FORGE_HOME";
pub const ENV_DATABASE_URL: &str = "FORGE_DATABASE_URL";
pub const ENV_URL: &str = "FORGE_URL";
pub const ENV_HOST: &str = "FORGE_HOST";
pub const ENV_PORT: &str = "FORGE_PORT";
pub const ENV_HARNESS_PATH: &str = "FORGE_HARNESS_PATH";
pub const ENV_CGROUP_ROOT: &str = "FORGE_CGROUP_ROOT";
const ENV_XDG_DATA_HOME: &str = "XDG_DATA_HOME";
const ENV_USER_HOME: &str = "HOME";
const ENV_XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";

// ---- Defaults (the only place these literals may live) ----------------------
/// Subdirectory of the XDG data directory holding the default home.
const DEFAULT_HOME_NAME: &str = "forge";
const DEFAULT_HOME_FALLBACK: &str = ".local/share";
pub const DEFAULT_HOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
pub const DEFAULT_PORT: u16 = 8110;
pub const DEFAULT_MAX_CONNECTIONS: u32 = 8;
pub const DEFAULT_CONNECT_TIMEOUT_SECONDS: u64 = 5;
pub const DEFAULT_WORK_CONCURRENCY: i32 = 2;
pub const DEFAULT_MODEL_CONCURRENCY: i32 = 2;
pub const DEFAULT_SERVER_CONCURRENCY: i32 = 1;
pub const DEFAULT_APPLICATION_PREFIX: &str = "forge";
pub const DEFAULT_SCHEMA_PREFIX: &str = "forge";
/// Default `PATH` of a harness runner that neither passes `PATH` through nor sets it.
pub const DEFAULT_HARNESS_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
pub const DEFAULT_CGROUP_ROOT: &str = "/sys/fs/cgroup";

// ---- systemd --user service (see service_unit.rs) -------------------------------
const DEFAULT_CONFIG_FALLBACK: &str = ".config";
const SYSTEMD_USER_SUBDIR: &str = "systemd/user";
/// Default unit name (without the `.service` suffix) when `--name` is not given.
pub const SERVICE_DEFAULT_NAME: &str = "agent-forge";
pub const SERVICE_SUFFIX: &str = ".service";
/// The service manager executable, looked up on PATH.
pub const SYSTEMCTL: &str = "systemctl";
/// First line of every generated unit; uninstall touches only files that carry it.
pub const SERVICE_MARKER: &str = "# Managed-By: agent-forge";
/// Where per-user systemd units live: `$XDG_CONFIG_HOME/systemd/user`, else
/// `$HOME/.config/systemd/user`.
pub fn systemd_user_dir(env: Env) -> Result<PathBuf> {
    let base = if let Some(xdg) = env(ENV_XDG_CONFIG_HOME).filter(|v| Path::new(v).is_absolute()) {
        PathBuf::from(xdg)
    } else if let Some(user) = env(ENV_USER_HOME).filter(|v| Path::new(v).is_absolute()) {
        Path::new(&user).join(DEFAULT_CONFIG_FALLBACK)
    } else {
        return refuse(format!(
            "cannot place a user unit: set {ENV_XDG_CONFIG_HOME} or {ENV_USER_HOME} to an absolute path"
        ));
    };
    Ok(base.join(SYSTEMD_USER_SUBDIR))
}

// ---- Home layout --------------------------------------------------------------
pub const CONFIG_FILE: &str = "config.json";
pub const RESTORE_MARKER: &str = ".restore-incomplete";
const LAYOUT_MARKER: &str = ".state-location";
const STATE_DIR: &str = "state";
const OUTPUTS_DIR: &str = "outputs";
const EFFECT_LOCKS_DIR: &str = "effect-locks";
const HARNESS_DIR: &str = "harness";
const LOGS_DIR: &str = "logs";
const PLUGINS_DIR: &str = "plugins";
const PLUGIN_STAGING_DIR: &str = "staging";
const PLUGIN_RELEASES_DIR: &str = "releases";
const CACHE_DIR: &str = "cache";
const IDENTITY_FILE: &str = "dbos-identity.json";
const IDENTITY_STAGING_FILE: &str = "dbos-identity.restore-new.json";
const ADMISSION_FILE: &str = "dbos-admission.sqlite";
const OWNER_LOCK_FILE: &str = "owner.lock";
const RESTORE_DUMP_FILE: &str = "restore-database.dump";
/// Suffix of the default backups directory, a sibling of the home.
const BACKUPS_SUFFIX: &str = "-backups";
const DEFAULT_BACKUPS_NAME: &str = "forge-backups";

/// Environment lookup; tests pass a closure over a map instead of the process.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;
/// The process environment; empty values count as unset.
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

// ---- config.json sections ----------------------------------------------------
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// PostgreSQL URL. Prefer `url_env` so the credential stays out of this file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Name of an environment variable that holds the PostgreSQL URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout_seconds: Option<u64>,
}
/// Locations relative to the home unless absolute. Unset keys use the defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PathsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_locks: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<PathBuf>,
    /// The sealed DBOS identity file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dbos_identity: Option<PathBuf>,
    /// The SQLite admission store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dbos_admission: Option<PathBuf>,
    /// Where backups started from the dashboard, CLI or MCP are written. Must lie
    /// outside the home (a backup never contains itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backups: Option<PathBuf>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<IpAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_http: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
    /// Service URL the CLI and MCP bridge call when no `--url`/`FORGE_URL` is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
}
/// DBOS identity and capacity. Names stay derived from the home ID; only the
/// prefix is configurable so one database can host many homes without collision.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DbosConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_concurrency: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_concurrency: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_concurrency: Option<i32>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    /// `PATH` for runners that do not pass `PATH` through or set `env.PATH`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_path: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContainmentConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_root: Option<PathBuf>,
}

/// Values taken from command-line flags (the highest precedence).
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    pub database_url: Option<String>,
    pub host: Option<IpAddr>,
    pub port: Option<u16>,
    pub url: Option<String>,
}

// ---- Home -----------------------------------------------------------------------
/// `--home`, then `FORGE_HOME`, then `$XDG_DATA_HOME/forge`, then `$HOME/.local/share/forge`.
pub fn resolve_home(flag: Option<PathBuf>, env: Env) -> Result<PathBuf> {
    let chosen = if let Some(path) = flag {
        path
    } else if let Some(value) = env(ENV_HOME) {
        PathBuf::from(value)
    } else if let Some(xdg) = env(ENV_XDG_DATA_HOME).filter(|v| Path::new(v).is_absolute()) {
        Path::new(&xdg).join(DEFAULT_HOME_NAME)
    } else if let Some(user) = env(ENV_USER_HOME).filter(|v| Path::new(v).is_absolute()) {
        Path::new(&user)
            .join(DEFAULT_HOME_FALLBACK)
            .join(DEFAULT_HOME_NAME)
    } else {
        return refuse(format!(
            "no Forge home: pass --home, set {ENV_HOME}, or set {ENV_XDG_DATA_HOME} or {ENV_USER_HOME}"
        ));
    };
    if chosen.as_os_str().is_empty() {
        return refuse("the Forge home path is empty");
    }
    Ok(std::path::absolute(chosen)?)
}
pub fn config_file(home: &Path) -> PathBuf {
    home.join(CONFIG_FILE)
}
pub fn restore_marker(home: &Path) -> PathBuf {
    home.join(RESTORE_MARKER)
}

// ---- Paths ----------------------------------------------------------------------
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
    pub state: PathBuf,
    pub outputs: PathBuf,
    pub effect_locks: PathBuf,
    pub logs: PathBuf,
    pub harness: PathBuf,
    pub plugins: PathBuf,
    pub cache: PathBuf,
    pub dbos_identity: PathBuf,
    pub dbos_admission: PathBuf,
    /// Online backups; outside the home. Created on first use, not by `ensure_dirs`.
    pub backups: PathBuf,
}
fn lexical(name: &str, raw: &Path, home: &Path) -> Result<PathBuf> {
    if raw.as_os_str().is_empty()
        || raw.to_str().is_none_or(|s| s.contains('\0'))
        || raw.components().any(|c| matches!(c, Component::ParentDir))
    {
        return refuse(format!(
            "paths.{name} must be a non-empty path without `..` components"
        ));
    }
    Ok(if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        home.join(raw)
    })
}
impl Paths {
    /// The default layout of one home, used when nothing overrides it.
    pub fn defaults(home: &Path) -> Self {
        Self::layout(home, PathsConfig::default())
    }
    fn layout(home: &Path, c: PathsConfig) -> Self {
        let state = c.state.unwrap_or_else(|| home.join(STATE_DIR));
        Self {
            outputs: c.outputs.unwrap_or_else(|| state.join(OUTPUTS_DIR)),
            effect_locks: c
                .effect_locks
                .unwrap_or_else(|| state.join(EFFECT_LOCKS_DIR)),
            harness: c.harness.unwrap_or_else(|| state.join(HARNESS_DIR)),
            logs: c.logs.unwrap_or_else(|| home.join(LOGS_DIR)),
            plugins: c.plugins.unwrap_or_else(|| home.join(PLUGINS_DIR)),
            cache: c.cache.unwrap_or_else(|| home.join(CACHE_DIR)),
            dbos_identity: c.dbos_identity.unwrap_or_else(|| state.join(IDENTITY_FILE)),
            dbos_admission: c
                .dbos_admission
                .unwrap_or_else(|| state.join(ADMISSION_FILE)),
            backups: c.backups.unwrap_or_else(|| {
                let name = home
                    .file_name()
                    .map(|n| format!("{}{BACKUPS_SUFFIX}", n.to_string_lossy()))
                    .unwrap_or_else(|| DEFAULT_BACKUPS_NAME.to_owned());
                home.with_file_name(name)
            }),
            state,
            home: home.to_path_buf(),
        }
    }
    /// Environment variables override config keys: `FORGE_<KEY>_DIR` for the directories.
    pub fn resolve(home: &Path, config: &PathsConfig, env: Env) -> Result<Self> {
        let pick =
            |name: &str, var: &str, configured: &Option<PathBuf>| -> Result<Option<PathBuf>> {
                let raw = env(var).map(PathBuf::from).or_else(|| configured.clone());
                raw.map(|raw| lexical(name, &raw, home)).transpose()
            };
        let file = |name: &str, configured: &Option<PathBuf>| {
            configured
                .as_deref()
                .map(|raw| lexical(name, raw, home))
                .transpose()
        };
        let resolved = PathsConfig {
            state: pick("state", "FORGE_STATE_DIR", &config.state)?,
            outputs: pick("outputs", "FORGE_OUTPUTS_DIR", &config.outputs)?,
            effect_locks: pick(
                "effect_locks",
                "FORGE_EFFECT_LOCKS_DIR",
                &config.effect_locks,
            )?,
            logs: pick("logs", "FORGE_LOGS_DIR", &config.logs)?,
            harness: pick("harness", "FORGE_HARNESS_DIR", &config.harness)?,
            plugins: pick("plugins", "FORGE_PLUGINS_DIR", &config.plugins)?,
            cache: pick("cache", "FORGE_CACHE_DIR", &config.cache)?,
            dbos_identity: file("dbos_identity", &config.dbos_identity)?,
            dbos_admission: file("dbos_admission", &config.dbos_admission)?,
            backups: pick("backups", "FORGE_BACKUPS_DIR", &config.backups)?,
        };
        Ok(Self::layout(home, resolved))
    }
    pub fn config_file(&self) -> PathBuf {
        config_file(&self.home)
    }
    /// Where a plugin install is copied and verified before it becomes a release.
    pub fn plugin_staging(&self) -> PathBuf {
        self.plugins.join(PLUGIN_STAGING_DIR)
    }
    /// Root of the immutable, digest-verified plugin releases.
    pub fn plugin_releases(&self) -> PathBuf {
        self.plugins.join(PLUGIN_RELEASES_DIR)
    }
    /// The immutable release directory of one installed plugin version.
    pub fn plugin_release(&self, id: &str, version: &str) -> PathBuf {
        self.plugin_releases().join(id).join(version)
    }
    pub fn restore_marker(&self) -> PathBuf {
        restore_marker(&self.home)
    }
    /// The exclusive executor lock, kept beside the state it guards.
    pub fn owner_lock(&self) -> PathBuf {
        self.state.join(OWNER_LOCK_FILE)
    }
    pub fn restore_dump(&self) -> PathBuf {
        self.state.join(RESTORE_DUMP_FILE)
    }
    pub fn identity_staging(&self) -> PathBuf {
        self.state.join(IDENTITY_STAGING_FILE)
    }
    /// The path relative to the home, or `None` when it lives outside it.
    pub fn relative(&self, path: &Path) -> Option<PathBuf> {
        path.strip_prefix(&self.home).ok().map(Path::to_path_buf)
    }
    /// Create every runtime directory privately (0700). Directories below the home
    /// are created level by level so no intermediate directory is left group- or
    /// world-accessible; an absolute location outside the home needs an existing parent
    /// only if its own ancestors are not creatable by this user.
    pub fn ensure_dirs(&self) -> Result<()> {
        crate::safety::private_mkdir(&self.home)?;
        let parent_of = |file: &Path| file.parent().map(Path::to_path_buf);
        for (name, dir) in [
            ("state", Some(self.state.clone())),
            ("outputs", Some(self.outputs.clone())),
            ("effect_locks", Some(self.effect_locks.clone())),
            ("logs", Some(self.logs.clone())),
            ("harness", Some(self.harness.clone())),
            ("plugins", Some(self.plugins.clone())),
            ("cache", Some(self.cache.clone())),
            ("dbos_identity", parent_of(&self.dbos_identity)),
            ("dbos_admission", parent_of(&self.dbos_admission)),
        ] {
            if let Some(dir) = dir {
                self.private_tree(&dir).map_err(|error| {
                    ConfigError(format!(
                        "paths.{name} ({}) cannot be used as a private directory: {error}; a symbolic link, a non-directory or a foreign-owned path is refused",
                        dir.display()
                    ))
                })?;
            }
        }
        Ok(())
    }
    fn private_tree(&self, dir: &Path) -> std::io::Result<()> {
        // `private_mkdir` walks every existing component (a link anywhere is refused)
        // and creates each missing level with mode 0700, below the home or outside it.
        crate::safety::private_mkdir(dir)
    }
    /// Where the durable state lives, recorded when a home first runs so that a later
    /// `paths.state` change is refused instead of silently starting an empty state.
    pub fn layout_marker(&self) -> PathBuf {
        self.home.join(LAYOUT_MARKER)
    }
    fn state_token(&self) -> String {
        self.relative(&self.state)
            .map(|p| format!("home:{}", p.display()))
            .unwrap_or_else(|| format!("abs:{}", self.state.display()))
    }
    /// Record the state location on first use; refuse a different one afterwards.
    pub fn check_layout(&self) -> Result<()> {
        let marker = self.layout_marker();
        let token = self.state_token();
        match std::fs::symlink_metadata(&marker) {
            Ok(_) => {
                let recorded = crate::safety::read_private(&marker)
                    .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
                    .map_err(|_| {
                        ConfigError(format!(
                            "{LAYOUT_MARKER} in the home is not one private regular file"
                        ))
                    })?;
                if recorded != token {
                    return refuse(format!(
                        "paths.state changed on an existing home (recorded {recorded}, configured {token}): the home's state (admission store, DBOS identity, outputs) lives at the recorded location. Restore the original paths.state/FORGE_STATE_DIR, or move the state directory yourself, stop the service and replace the `{LAYOUT_MARKER}` file in the home with the new location."
                    ));
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                crate::safety::write_new(&marker, format!("{token}\n").as_bytes()).map_err(
                    |_| ConfigError(format!("cannot record {LAYOUT_MARKER} in the home")),
                )?;
                Ok(())
            }
            Err(_) => refuse(format!("{LAYOUT_MARKER} in the home cannot be inspected")),
        }
    }
}

// ---- Settings --------------------------------------------------------------------
#[derive(Clone)]
pub struct Database {
    url: Option<String>,
    missing: String,
    pub max_connections: u32,
    pub connect_timeout: Duration,
}
impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("url", &self.url.as_ref().map(|_| "[redacted]"))
            .field("max_connections", &self.max_connections)
            .field("connect_timeout", &self.connect_timeout)
            .finish()
    }
}
impl Database {
    /// The resolved PostgreSQL URL. There is no default: a missing one is an error
    /// that explains how to set it.
    pub fn url(&self) -> Result<&str> {
        match &self.url {
            Some(url) => Ok(url),
            None => refuse(self.missing.clone()),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub host: IpAddr,
    pub port: u16,
    pub mcp_http: bool,
    pub tls: Option<TlsConfig>,
    /// The URL the CLI and MCP bridge call.
    pub url: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dbos {
    pub application_prefix: String,
    pub schema_prefix: String,
    pub work_concurrency: i32,
    pub model_concurrency: i32,
    pub server_concurrency: i32,
}
#[derive(Clone, Debug)]
pub struct Settings {
    pub paths: Paths,
    pub database: Database,
    pub server: Server,
    pub dbos: Dbos,
    pub harness_default_path: String,
    pub cgroup_root: PathBuf,
    /// `database.url_env`: the variable holding the database URL, withheld from plugins.
    pub database_url_env: Option<String>,
    /// Where each reported setting came from: `flag`, `env`, `config` or `default`.
    pub sources: Vec<(&'static str, &'static str)>,
}

/// Every `config.json` key the dashboard, CLI and MCP may edit. `database.url` is
/// deliberately absent: a literal credential is never accepted through the service.
pub const EDITABLE: &[&str] = &[
    "database.url_env",
    "database.max_connections",
    "database.connect_timeout_seconds",
    "server.host",
    "server.port",
    "server.url",
    "server.mcp_http",
    "dbos.application_prefix",
    "dbos.schema_prefix",
    "dbos.work_concurrency",
    "dbos.model_concurrency",
    "dbos.server_concurrency",
    "harness.default_path",
    "containment.cgroup_root",
    "paths.state",
    "paths.outputs",
    "paths.effect_locks",
    "paths.logs",
    "paths.harness",
    "paths.plugins",
    "paths.cache",
    "paths.dbos_identity",
    "paths.dbos_admission",
    "paths.backups",
];

fn name_ok(value: &str, allow_dash: bool) -> bool {
    (1..=16).contains(&value.len())
        && value.as_bytes()[0].is_ascii_lowercase()
        && value.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || (allow_dash && b == b'-')
        })
}
fn env_name_ok(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.as_bytes()[0].is_ascii_digit()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
fn server_url(host: IpAddr, port: u16, scheme: &str) -> String {
    match host {
        IpAddr::V4(v4) => format!("{scheme}://{v4}:{port}"),
        IpAddr::V6(v6) => format!("{scheme}://[{v6}]:{port}"),
    }
}
fn server_settings(config: &ServerConfig, o: &Overrides, env: Env) -> Result<Server> {
    let host: IpAddr = match (o.host, env(ENV_HOST)) {
        (Some(host), _) => host,
        (None, Some(raw)) => raw
            .parse()
            .or_else(|_| refuse(format!("{ENV_HOST} must be a numeric IP address")))?,
        (None, None) => config.host.unwrap_or(DEFAULT_HOST),
    };
    if !host.is_loopback() {
        return refuse(format!(
            "server host {host} is not a loopback address: Forge serves only 127.0.0.0/8 or ::1 (put an authenticating proxy in front for remote access)"
        ));
    }
    let port = match (o.port, env(ENV_PORT)) {
        (Some(port), _) => port,
        (None, Some(raw)) => raw
            .parse()
            .or_else(|_| refuse(format!("{ENV_PORT} must be a port number")))?,
        (None, None) => config.port.unwrap_or(DEFAULT_PORT),
    };
    let scheme = if let Some(tls) = &config.tls {
        if !tls.certificate.is_absolute() || !tls.private_key.is_absolute() {
            return refuse("server.tls certificate and private_key must use absolute paths");
        }
        "https"
    } else {
        "http"
    };
    let url = o
        .url
        .clone()
        .or_else(|| env(ENV_URL))
        .or_else(|| config.url.clone())
        .unwrap_or_else(|| server_url(host, port, scheme));
    Ok(Server {
        host,
        port,
        url,
        mcp_http: config.mcp_http.unwrap_or(false),
        tls: config.tls.clone(),
    })
}
fn bounded<T: PartialOrd + std::fmt::Display + Copy>(
    name: &str,
    value: T,
    low: T,
    high: T,
) -> Result<T> {
    if value < low || value > high {
        return refuse(format!("{name} must be between {low} and {high}"));
    }
    Ok(value)
}

impl Settings {
    /// Resolve everything for one home from flags, environment and its config.
    pub fn resolve(home: PathBuf, policy: &Policy, o: &Overrides, env: Env) -> Result<Self> {
        let paths = Paths::resolve(&home, &policy.paths, env)?;
        let db = &policy.database;
        if db.url.is_some() && db.url_env.is_some() {
            return refuse("database.url and database.url_env are mutually exclusive");
        }
        if let Some(name) = &db.url_env
            && !env_name_ok(name)
        {
            return refuse("database.url_env must be an environment variable name");
        }
        let flag_or_env = o.database_url.clone().or_else(|| env(ENV_DATABASE_URL));
        let (url, missing) = if let Some(url) = flag_or_env.or_else(|| db.url.clone()) {
            (Some(url), String::new())
        } else if let Some(name) = &db.url_env {
            (
                env(name),
                format!(
                    "database.url_env in config.json names {name}, which is not set in the environment"
                ),
            )
        } else {
            (
                None,
                format!(
                    "no PostgreSQL database configured: pass --database-url, set {ENV_DATABASE_URL}, or add database.url or database.url_env to {CONFIG_FILE} in the Forge home"
                ),
            )
        };
        let database = Database {
            url,
            missing,
            max_connections: bounded(
                "database.max_connections",
                db.max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS),
                2,
                100,
            )?,
            connect_timeout: Duration::from_secs(bounded(
                "database.connect_timeout_seconds",
                db.connect_timeout_seconds
                    .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECONDS),
                1,
                300,
            )?),
        };
        let d = &policy.dbos;
        let dbos = Dbos {
            application_prefix: d
                .application_prefix
                .clone()
                .unwrap_or_else(|| DEFAULT_APPLICATION_PREFIX.into()),
            schema_prefix: d
                .schema_prefix
                .clone()
                .unwrap_or_else(|| DEFAULT_SCHEMA_PREFIX.into()),
            work_concurrency: d.work_concurrency.unwrap_or(DEFAULT_WORK_CONCURRENCY),
            model_concurrency: d.model_concurrency.unwrap_or(DEFAULT_MODEL_CONCURRENCY),
            server_concurrency: d.server_concurrency.unwrap_or(DEFAULT_SERVER_CONCURRENCY),
        };
        if !name_ok(&dbos.application_prefix, true) || !name_ok(&dbos.schema_prefix, false) {
            return refuse(
                "dbos.application_prefix and dbos.schema_prefix must be short lowercase names",
            );
        }
        let harness_default_path = env(ENV_HARNESS_PATH)
            .or_else(|| policy.harness.default_path.clone())
            .unwrap_or_else(|| DEFAULT_HARNESS_PATH.into());
        if harness_default_path.is_empty() || harness_default_path.contains('\0') {
            return refuse("harness.default_path must be a non-empty PATH value");
        }
        let cgroup_root = env(ENV_CGROUP_ROOT)
            .map(PathBuf::from)
            .or_else(|| policy.containment.cgroup_root.clone())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CGROUP_ROOT));
        if !cgroup_root.is_absolute() {
            return refuse("containment.cgroup_root must be an absolute path");
        }
        let source = |flag: bool, var: Option<&str>, configured: bool| -> &'static str {
            if flag {
                "flag"
            } else if var.is_some_and(|v| env(v).is_some()) {
                "env"
            } else if configured {
                "config"
            } else {
                "default"
            }
        };
        let (p, s, d) = (&policy.paths, &policy.server, &policy.dbos);
        let path_source = |var: &str, set: bool| source(false, Some(var), set);
        let sources = vec![
            (
                "database.url",
                if o.database_url.is_some() {
                    "flag"
                } else if env(ENV_DATABASE_URL).is_some() {
                    "env"
                } else if db.url.is_some() || db.url_env.is_some() {
                    "config"
                } else {
                    "default"
                },
            ),
            (
                "database.url_env",
                source(false, None, db.url_env.is_some()),
            ),
            (
                "database.max_connections",
                source(false, None, db.max_connections.is_some()),
            ),
            (
                "database.connect_timeout_seconds",
                source(false, None, db.connect_timeout_seconds.is_some()),
            ),
            (
                "server.host",
                source(o.host.is_some(), Some(ENV_HOST), s.host.is_some()),
            ),
            (
                "server.port",
                source(o.port.is_some(), Some(ENV_PORT), s.port.is_some()),
            ),
            (
                "server.url",
                source(o.url.is_some(), Some(ENV_URL), s.url.is_some()),
            ),
            ("server.mcp_http", source(false, None, s.mcp_http.is_some())),
            ("server.tls", source(false, None, s.tls.is_some())),
            (
                "dbos.application_prefix",
                source(false, None, d.application_prefix.is_some()),
            ),
            (
                "dbos.schema_prefix",
                source(false, None, d.schema_prefix.is_some()),
            ),
            (
                "dbos.work_concurrency",
                source(false, None, d.work_concurrency.is_some()),
            ),
            (
                "dbos.model_concurrency",
                source(false, None, d.model_concurrency.is_some()),
            ),
            (
                "dbos.server_concurrency",
                source(false, None, d.server_concurrency.is_some()),
            ),
            (
                "harness.default_path",
                source(
                    false,
                    Some(ENV_HARNESS_PATH),
                    policy.harness.default_path.is_some(),
                ),
            ),
            (
                "containment.cgroup_root",
                source(
                    false,
                    Some(ENV_CGROUP_ROOT),
                    policy.containment.cgroup_root.is_some(),
                ),
            ),
            (
                "paths.state",
                path_source("FORGE_STATE_DIR", p.state.is_some()),
            ),
            (
                "paths.outputs",
                path_source("FORGE_OUTPUTS_DIR", p.outputs.is_some()),
            ),
            (
                "paths.effect_locks",
                path_source("FORGE_EFFECT_LOCKS_DIR", p.effect_locks.is_some()),
            ),
            (
                "paths.logs",
                path_source("FORGE_LOGS_DIR", p.logs.is_some()),
            ),
            (
                "paths.harness",
                path_source("FORGE_HARNESS_DIR", p.harness.is_some()),
            ),
            (
                "paths.plugins",
                path_source("FORGE_PLUGINS_DIR", p.plugins.is_some()),
            ),
            (
                "paths.cache",
                path_source("FORGE_CACHE_DIR", p.cache.is_some()),
            ),
            (
                "paths.dbos_identity",
                source(false, None, p.dbos_identity.is_some()),
            ),
            (
                "paths.dbos_admission",
                source(false, None, p.dbos_admission.is_some()),
            ),
            (
                "paths.backups",
                path_source("FORGE_BACKUPS_DIR", p.backups.is_some()),
            ),
        ];
        Ok(Self {
            paths,
            database,
            server: server_settings(&policy.server, o, env)?,
            dbos,
            harness_default_path,
            cgroup_root,
            database_url_env: db.url_env.clone(),
            sources,
        })
    }
    /// The effective value of every reported setting with its source. Secrets are
    /// never included: a database URL is only reported as set or unset.
    pub fn report(&self, policy: &Policy) -> Vec<serde_json::Value> {
        use serde_json::json;
        let path = |p: &Path| json!(p.to_string_lossy());
        let value = |key: &str| -> serde_json::Value {
            match key {
                "database.url" => json!(if self.database.url.is_some() {
                    "[redacted]"
                } else {
                    "(not set)"
                }),
                "database.url_env" => json!(policy.database.url_env),
                "database.max_connections" => json!(self.database.max_connections),
                "database.connect_timeout_seconds" => {
                    json!(self.database.connect_timeout.as_secs())
                }
                "server.host" => json!(self.server.host.to_string()),
                "server.port" => json!(self.server.port),
                "server.url" => json!(self.server.url),
                "server.mcp_http" => json!(self.server.mcp_http),
                "server.tls" => json!(self.server.tls.is_some()),
                "dbos.application_prefix" => json!(self.dbos.application_prefix),
                "dbos.schema_prefix" => json!(self.dbos.schema_prefix),
                "dbos.work_concurrency" => json!(self.dbos.work_concurrency),
                "dbos.model_concurrency" => json!(self.dbos.model_concurrency),
                "dbos.server_concurrency" => json!(self.dbos.server_concurrency),
                "harness.default_path" => json!(self.harness_default_path),
                "containment.cgroup_root" => path(&self.cgroup_root),
                "paths.state" => path(&self.paths.state),
                "paths.outputs" => path(&self.paths.outputs),
                "paths.effect_locks" => path(&self.paths.effect_locks),
                "paths.logs" => path(&self.paths.logs),
                "paths.harness" => path(&self.paths.harness),
                "paths.plugins" => path(&self.paths.plugins),
                "paths.cache" => path(&self.paths.cache),
                "paths.dbos_identity" => path(&self.paths.dbos_identity),
                "paths.dbos_admission" => path(&self.paths.dbos_admission),
                "paths.backups" => path(&self.paths.backups),
                _ => serde_json::Value::Null,
            }
        };
        let mut rows = vec![
            json!({"key":"home","value":path(&self.paths.home),"source":"startup","editable":false,"secret":false}),
        ];
        rows.extend(self.sources.iter().map(|(key, source)| {
            json!({"key":key,"value":value(key),"source":source,"editable":EDITABLE.contains(key),"secret":*key=="database.url"})
        }));
        rows
    }
    /// The DBOS runtime identity bound to the policy's home and this database.
    pub fn runtime_config(&self, policy: &Policy) -> Result<crate::dbos::RuntimeConfig> {
        let id = &policy.home_id;
        Ok(crate::dbos::RuntimeConfig::from_json(
            &serde_json::json!({
                "home_id": id,
                "application": format!("{}-{id}", self.dbos.application_prefix),
                "schema": format!("{}_{id}", self.dbos.schema_prefix),
                "app_version": crate::core::VERSION,
                "executor_id": format!("forge-v1-{id}"),
                "database_url": self.database.url()?,
                "work_concurrency": self.dbos.work_concurrency,
                "model_concurrency": self.dbos.model_concurrency,
                "server_concurrency": self.dbos.server_concurrency,
                "max_connections": self.database.max_connections,
                "connect_timeout_seconds": self.database.connect_timeout.as_secs(),
            })
            .to_string(),
        )?)
    }
    /// Publish the process-wide values that code without a settings handle reads.
    pub fn install(&self) {
        if let Ok(mut ambient) = AMBIENT.write() {
            *ambient = Some((
                self.harness_default_path.clone(),
                self.cgroup_root.clone(),
                self.database_url_env.iter().cloned().collect(),
            ));
        }
    }
    #[cfg(test)]
    pub fn for_tests(policy: &Policy) -> Self {
        let url = "postgresql://test@127.0.0.1:1/test".to_owned();
        let o = Overrides {
            database_url: Some(url),
            ..Default::default()
        };
        Self::resolve(PathBuf::from("/nonexistent-test-home"), policy, &o, &|_| {
            None
        })
        .unwrap()
    }
}

/// The service URL a client command should call. Reads only the `server` section
/// of the home's config, and only when neither `--url` nor `FORGE_URL` decides it.
pub fn client_url(home: &Path, o: &Overrides, env: Env) -> Result<String> {
    let mut server = ServerConfig::default();
    if o.url.is_none()
        && env(ENV_URL).is_none()
        && let Ok(bytes) = crate::safety::read_private(&config_file(home))
        && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let Some(section) = value.get("server")
    {
        server = serde_json::from_value(section.clone())
            .or_else(|_| refuse("config.json server section is invalid"))?;
    }
    Ok(server_settings(&server, o, env)?.url)
}

// ---- Process-wide values ---------------------------------------------------------------
/// (harness default PATH, cgroup root, environment variables withheld from plugins).
type Ambient = (String, PathBuf, Vec<String>);
static AMBIENT: RwLock<Option<Ambient>> = RwLock::new(None);
/// Whether `name` is the variable that holds a credential Forge itself uses (the
/// target of `database.url_env`), which no plugin may be given.
pub fn withheld_env(name: &str) -> bool {
    name == crate::workflow_credentials::DIRECTORY_ENV
        || AMBIENT
            .read()
            .ok()
            .and_then(|a| a.as_ref().map(|(_, _, w)| w.iter().any(|n| n == name)))
            .unwrap_or(false)
}
/// `PATH` of a runner environment that does not otherwise define one.
pub fn harness_default_path() -> String {
    AMBIENT
        .read()
        .ok()
        .and_then(|a| a.as_ref().map(|(path, _, _)| path.clone()))
        .unwrap_or_else(|| DEFAULT_HARNESS_PATH.into())
}
/// Root of the cgroup v2 hierarchy used for process containment.
pub fn cgroup_root() -> PathBuf {
    AMBIENT
        .read()
        .ok()
        .and_then(|a| a.as_ref().map(|(_, root, _)| root.clone()))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CGROUP_ROOT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }
    fn policy(extra: &str) -> Policy {
        let raw =
            format!(r#"{{"home_id":"0123456789abcdef0123456789abcdef","actors":{{}}{extra}}}"#);
        serde_json::from_str(&raw).unwrap()
    }
    fn resolve(policy: &Policy, o: &Overrides, env: &[(&str, &str)]) -> Result<Settings> {
        Settings::resolve(PathBuf::from("/h"), policy, o, &env_of(env))
    }

    #[test]
    fn home_precedence_flag_env_xdg_then_user_home() {
        let all = env_of(&[
            (ENV_HOME, "/env/home"),
            (ENV_XDG_DATA_HOME, "/xdg"),
            (ENV_USER_HOME, "/u"),
        ]);
        assert_eq!(
            resolve_home(Some("/flag".into()), &all).unwrap(),
            Path::new("/flag")
        );
        assert_eq!(resolve_home(None, &all).unwrap(), Path::new("/env/home"));
        let xdg = env_of(&[(ENV_XDG_DATA_HOME, "/xdg"), (ENV_USER_HOME, "/u")]);
        assert_eq!(resolve_home(None, &xdg).unwrap(), Path::new("/xdg/forge"));
        let user = env_of(&[(ENV_USER_HOME, "/u")]);
        assert_eq!(
            resolve_home(None, &user).unwrap(),
            Path::new("/u/.local/share/forge")
        );
        // A relative XDG value is ignored, as the specification requires.
        let relative = env_of(&[(ENV_XDG_DATA_HOME, "rel"), (ENV_USER_HOME, "/u")]);
        assert_eq!(
            resolve_home(None, &relative).unwrap(),
            Path::new("/u/.local/share/forge")
        );
        assert!(resolve_home(None, &env_of(&[])).is_err());
    }

    #[test]
    fn database_precedence_flag_env_config_url_then_url_env() {
        let configured = policy(r#","database":{"url":"postgresql://cfg@h/c"}"#);
        let flag = Overrides {
            database_url: Some("postgresql://flag@h/f".into()),
            ..Default::default()
        };
        let both = [(ENV_DATABASE_URL, "postgresql://env@h/e")];
        let s = resolve(&configured, &flag, &both).unwrap();
        assert_eq!(s.database.url().unwrap(), "postgresql://flag@h/f");
        let s = resolve(&configured, &Overrides::default(), &both).unwrap();
        assert_eq!(s.database.url().unwrap(), "postgresql://env@h/e");
        let s = resolve(&configured, &Overrides::default(), &[]).unwrap();
        assert_eq!(s.database.url().unwrap(), "postgresql://cfg@h/c");
    }

    #[test]
    fn url_env_reads_the_named_variable_and_reports_when_it_is_unset() {
        let configured = policy(r#","database":{"url_env":"MY_FORGE_DB"}"#);
        let s = resolve(
            &configured,
            &Overrides::default(),
            &[("MY_FORGE_DB", "postgresql://named@h/n")],
        )
        .unwrap();
        assert_eq!(s.database.url().unwrap(), "postgresql://named@h/n");
        let error = resolve(&configured, &Overrides::default(), &[])
            .unwrap()
            .database
            .url()
            .unwrap_err()
            .to_string();
        assert!(error.contains("MY_FORGE_DB") && error.contains("not set"));
        // FORGE_DATABASE_URL outranks url_env.
        let s = resolve(
            &configured,
            &Overrides::default(),
            &[
                ("MY_FORGE_DB", "postgresql://named@h/n"),
                (ENV_DATABASE_URL, "postgresql://env@h/e"),
            ],
        )
        .unwrap();
        assert_eq!(s.database.url().unwrap(), "postgresql://env@h/e");
        assert!(
            resolve(
                &policy(r#","database":{"url":"postgresql://a@h/a","url_env":"X"}"#),
                &Overrides::default(),
                &[]
            )
            .is_err()
        );
        assert!(
            resolve(
                &policy(r#","database":{"url_env":"not a name"}"#),
                &Overrides::default(),
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn a_missing_database_fails_clearly_and_never_defaults() {
        let s = resolve(&policy(""), &Overrides::default(), &[]).unwrap();
        let error = s.database.url().unwrap_err().to_string();
        for hint in ["--database-url", ENV_DATABASE_URL, "database.url_env"] {
            assert!(error.contains(hint), "{error}");
        }
        assert!(s.runtime_config(&policy("")).is_err());
    }

    #[test]
    fn database_pool_and_timeout_default_and_are_bounded() {
        let s = resolve(&policy(""), &Overrides::default(), &[]).unwrap();
        assert_eq!(s.database.max_connections, DEFAULT_MAX_CONNECTIONS);
        assert_eq!(s.database.connect_timeout.as_secs(), 5);
        let tuned = policy(r#","database":{"max_connections":4,"connect_timeout_seconds":20}"#);
        let s = resolve(&tuned, &Overrides::default(), &[]).unwrap();
        assert_eq!(s.database.max_connections, 4);
        assert_eq!(s.database.connect_timeout.as_secs(), 20);
        assert!(
            resolve(
                &policy(r#","database":{"max_connections":1}"#),
                &Overrides::default(),
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn paths_default_under_home_and_relative_values_resolve_against_home() {
        let s = resolve(&policy(""), &Overrides::default(), &[]).unwrap();
        assert_eq!(s.paths, Paths::defaults(Path::new("/h")));
        assert_eq!(s.paths.state, Path::new("/h/state"));
        assert_eq!(s.paths.outputs, Path::new("/h/state/outputs"));
        assert_eq!(s.paths.plugins, Path::new("/h/plugins"));
        let custom = policy(
            r#","paths":{"state":"data/st","logs":"/var/log/forge","dbos_admission":"adm/a.sqlite"}"#,
        );
        let s = resolve(&custom, &Overrides::default(), &[]).unwrap();
        assert_eq!(s.paths.state, Path::new("/h/data/st"));
        // Dependent defaults follow the relocated state directory.
        assert_eq!(s.paths.outputs, Path::new("/h/data/st/outputs"));
        assert_eq!(s.paths.harness, Path::new("/h/data/st/harness"));
        assert_eq!(
            s.paths.dbos_identity,
            Path::new("/h/data/st/dbos-identity.json")
        );
        assert_eq!(s.paths.logs, Path::new("/var/log/forge"));
        assert_eq!(s.paths.dbos_admission, Path::new("/h/adm/a.sqlite"));
        assert_eq!(s.paths.relative(&s.paths.state), Some("data/st".into()));
        assert_eq!(s.paths.relative(&s.paths.logs), None);
        // Environment beats config; a relative env value also resolves against home.
        let s = resolve(
            &custom,
            &Overrides::default(),
            &[("FORGE_STATE_DIR", "other")],
        )
        .unwrap();
        assert_eq!(s.paths.state, Path::new("/h/other"));
        assert!(
            resolve(
                &policy(r#","paths":{"cache":"../escape"}"#),
                &Overrides::default(),
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn ensure_dirs_creates_private_directories_below_the_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("h");
        let custom = policy(r#","paths":{"state":"a/b","cache":"c/d"}"#);
        let s = Settings::resolve(home.clone(), &custom, &Overrides::default(), &|_| None).unwrap();
        s.paths.ensure_dirs().unwrap();
        use std::os::unix::fs::PermissionsExt;
        for dir in [
            &s.paths.state,
            &s.paths.outputs,
            &s.paths.effect_locks,
            &s.paths.harness,
            &s.paths.logs,
            &s.paths.plugins,
            &s.paths.cache,
            &home.join("a"),
            &home.join("c"),
        ] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "{dir:?}");
        }
    }

    #[test]
    fn server_precedence_and_client_url_derivation() {
        let configured = policy(r#","server":{"host":"127.0.0.2","port":9001}"#);
        let s = resolve(&configured, &Overrides::default(), &[]).unwrap();
        assert_eq!(
            (s.server.host.to_string(), s.server.port),
            ("127.0.0.2".into(), 9001)
        );
        assert_eq!(s.server.url, "http://127.0.0.2:9001");
        let s = resolve(&configured, &Overrides::default(), &[(ENV_PORT, "9002")]).unwrap();
        assert_eq!(s.server.port, 9002);
        let flag = Overrides {
            port: Some(9003),
            ..Default::default()
        };
        let s = resolve(&configured, &flag, &[(ENV_PORT, "9002")]).unwrap();
        assert_eq!(s.server.port, 9003);
        let s = resolve(&policy(""), &Overrides::default(), &[]).unwrap();
        assert_eq!(s.server.url, "http://127.0.0.1:8110");
        let s = resolve(
            &policy(""),
            &Overrides::default(),
            &[(ENV_URL, "http://x:1")],
        )
        .unwrap();
        assert_eq!(s.server.url, "http://x:1");
        let v6 = policy(r#","server":{"host":"::1"}"#);
        assert_eq!(
            resolve(&v6, &Overrides::default(), &[]).unwrap().server.url,
            "http://[::1]:8110"
        );
        assert!(resolve(&policy(""), &Overrides::default(), &[(ENV_PORT, "x")]).is_err());
    }

    #[test]
    fn client_url_reads_only_the_server_section_of_an_existing_config() {
        let home = tempfile::tempdir().unwrap();
        let none = |_: &str| None;
        assert_eq!(
            client_url(home.path(), &Overrides::default(), &none).unwrap(),
            "http://127.0.0.1:8110"
        );
        crate::safety::write_new(
            &config_file(home.path()),
            br#"{"server":{"port":9100},"unrelated":1}"#,
        )
        .unwrap();
        assert_eq!(
            client_url(home.path(), &Overrides::default(), &none).unwrap(),
            "http://127.0.0.1:9100"
        );
        let flag = Overrides {
            url: Some("http://flag:1".into()),
            ..Default::default()
        };
        assert_eq!(
            client_url(home.path(), &flag, &none).unwrap(),
            "http://flag:1"
        );
    }

    #[test]
    fn dbos_identity_prefixes_and_runtime_config_follow_settings() {
        let p = policy(
            r#","dbos":{"application_prefix":"alpha","schema_prefix":"alpha_s","work_concurrency":3}"#,
        );
        let o = Overrides {
            database_url: Some("postgresql://u@127.0.0.1:5432/db".into()),
            ..Default::default()
        };
        let s = resolve(&p, &o, &[]).unwrap();
        let rc = s.runtime_config(&p).unwrap();
        assert_eq!(rc.application, "alpha-0123456789abcdef0123456789abcdef");
        assert_eq!(rc.schema, "alpha_s_0123456789abcdef0123456789abcdef");
        assert_eq!(rc.work_concurrency, 3);
        assert_eq!(rc.max_connections, DEFAULT_MAX_CONNECTIONS);
        let defaults = policy("");
        let s = resolve(&defaults, &o, &[]).unwrap();
        let rc = s.runtime_config(&defaults).unwrap();
        assert_eq!(rc.application, "forge-0123456789abcdef0123456789abcdef");
        assert_eq!(rc.schema, "forge_0123456789abcdef0123456789abcdef");
        assert!(resolve(&policy(r#","dbos":{"schema_prefix":"Bad-Name"}"#), &o, &[]).is_err());
    }

    #[test]
    fn harness_path_and_cgroup_root_resolve_with_precedence() {
        let s = resolve(&policy(""), &Overrides::default(), &[]).unwrap();
        assert_eq!(s.harness_default_path, DEFAULT_HARNESS_PATH);
        assert_eq!(s.cgroup_root, Path::new(DEFAULT_CGROUP_ROOT));
        let p = policy(
            r#","harness":{"default_path":"/opt/bin:/bin"},"containment":{"cgroup_root":"/cg"}"#,
        );
        let s = resolve(&p, &Overrides::default(), &[]).unwrap();
        assert_eq!(s.harness_default_path, "/opt/bin:/bin");
        assert_eq!(s.cgroup_root, Path::new("/cg"));
        let s = resolve(&p, &Overrides::default(), &[(ENV_HARNESS_PATH, "/env/bin")]).unwrap();
        assert_eq!(s.harness_default_path, "/env/bin");
    }

    #[test]
    fn existing_configs_without_the_new_sections_load_unchanged() {
        let legacy = r#"{"home_id":"0123456789abcdef0123456789abcdef","actors":{"operator":{"token":"t","capabilities":["read"],"types":["echo"]}},"models":{},"plugins":{},"runners":{}}"#;
        let p: Policy = serde_json::from_str(legacy).unwrap();
        let s =
            Settings::resolve(PathBuf::from("/h"), &p, &Overrides::default(), &|_| None).unwrap();
        assert_eq!(s.paths, Paths::defaults(Path::new("/h")));
        assert_eq!(s.server.url, "http://127.0.0.1:8110");
        assert_eq!(s.dbos.work_concurrency, 2);
        // Serializing back adds none of the new empty sections.
        let again = serde_json::to_value(&p).unwrap();
        for key in [
            "database",
            "paths",
            "server",
            "dbos",
            "harness",
            "containment",
        ] {
            assert!(again.get(key).is_none(), "{key}");
        }
        // The new sections are strict: an unknown key is refused.
        assert!(
            serde_json::from_str::<Policy>(
                r#"{"home_id":"0123456789abcdef0123456789abcdef","actors":{},"paths":{"bogus":"x"}}"#
            )
            .is_err()
        );
    }
}

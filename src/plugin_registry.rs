//! Installed plugin lifecycle: install, enable, disable, uninstall, and the runtime
//! surfaces (CLI, HTTP API, MCP, dashboard assets, jobs) that an enabled plugin adds.
//!
//! Design (see docs/plugins.md): a plugin release is an immutable, digest-verified
//! directory `<paths.plugins>/releases/<id>/<version>`. The registry of record is the
//! `forge_plugins` table in the admission store, so visibility and enableability are
//! decided by one crash-safe SQLite transaction. An install is staged, verified and
//! atomically renamed *before* its row is written; a release without a row is an
//! orphan that startup removes, so a half-finished install is never visible. DBOS stays
//! the only workflow runner: jobs and workflows are materialized into the ordinary work
//! store and schedules and run by the existing workflows.
use crate::{
    core::{Core, Denied, Invalid},
    plugin::{self, Plugin, PluginConfig},
    plugin_manifest::{
        self as manifest, ApiMeta, Kind, Manifest, McpMeta, ResourceKind, WorkResource,
    },
    safety,
    settings::Paths,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

const MAX_FILES: usize = 512;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TREE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DEPTH: usize = 12;
const MAX_CLI_OUTPUT: usize = 256 * 1024;
const MAX_QUERY_PARAMS: usize = 64;
const ACTIVE: [&str; 4] = ["ADMITTED", "PENDING", "ENQUEUED", "DELAYED"];
/// The only response headers a plugin API route may set.
const RESPONSE_HEADERS: [&str; 3] = ["content-type", "cache-control", "etag"];
/// The only content types a plugin API route may return (never HTML).
const RESPONSE_TYPES: [&str; 3] = ["application/json", "text/plain", "text/csv"];
const SCAN_RUNS: i64 = 2000;

/// A refused lifecycle or surface request; the message is value-free and safe to show.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PluginError(pub String);
fn refuse<T>(message: impl Into<String>) -> Result<T> {
    Err(PluginError(message.into()).into())
}
/// A bounded label for the audit target of an install: the last path component only.
fn source_label(source: &str) -> &str {
    let name = source.rsplit('/').find(|p| !p.is_empty()).unwrap_or("");
    &name[..name.floor_char_boundary(64)]
}

/// Where a simulated crash stops an install (tests); nothing after it runs, not even cleanup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CrashAt {
    AfterStage,
    AfterRename,
}
impl CrashAt {
    #[cfg(feature = "test-adapters")]
    fn name(self) -> &'static str {
        match self {
            CrashAt::AfterStage => "after_stage",
            CrashAt::AfterRename => "after_rename",
        }
    }
}
fn crash_point(at: CrashAt, injected: Option<CrashAt>) -> Result<()> {
    if injected == Some(at) {
        return refuse("simulated crash");
    }
    // Live proof only: a real SIGKILL-equivalent at a named install stage.
    #[cfg(feature = "test-adapters")]
    if std::env::var("FORGE_PLUGIN_FAILPOINT").as_deref() == Ok(at.name()) {
        std::process::abort();
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Installed and verified, not dispatchable.
    Installed,
    Enabled,
    /// A disable is checking for activity; nothing new may dispatch.
    Disabling,
    /// The release is missing or fails its digests, or the plugin does not describe itself.
    Broken,
}
impl Phase {
    fn label(self) -> &'static str {
        match self {
            Phase::Installed => "disabled",
            Phase::Enabled => "enabled",
            Phase::Disabling => "disabling",
            Phase::Broken => "broken",
        }
    }
}

pub(crate) struct Entry {
    manifest: Manifest,
    manifest_sha256: String,
    phase: Phase,
    plugin: Option<Plugin>,
    problem: Option<String>,
    installed: f64,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<String, Entry>,
    /// Materialized work id -> owning plugin id.
    owned_work: BTreeMap<String, String>,
}

/// In-memory registry projection plus lifecycle serialization.
#[derive(Default)]
pub struct Registry {
    state: RwLock<State>,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
    /// Direct (CLI/API/MCP) plugin calls in flight, per plugin id.
    inflight: Mutex<BTreeMap<String, usize>>,
}

impl Registry {
    pub(crate) async fn lock_lifecycle(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.lifecycle.clone().lock_owned().await
    }
}

/// Counts one in-flight direct plugin call so a disable can refuse while it runs.
pub(crate) struct CallGuard {
    registry: Arc<Registry>,
    id: String,
}
impl Drop for CallGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.registry.inflight.lock()
            && let Some(count) = map.get_mut(&self.id)
        {
            *count = count.saturating_sub(1);
        }
    }
}

// ---- Filesystem layer -----------------------------------------------------------------
fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| "plugin file could not be opened")?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > limit {
        return refuse("plugin files must be regular files within the size limit");
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return refuse("plugin file exceeds the size limit");
    }
    Ok(bytes)
}

/// Relative paths of every regular file below `root` (symlinks and special files refuse).
fn walk(root: &Path) -> Result<Vec<(String, u32, u64)>> {
    fn visit(
        root: &Path,
        dir: &Path,
        depth: usize,
        out: &mut Vec<(String, u32, u64)>,
        total: &mut u64,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return refuse("plugin tree is nested too deeply");
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                visit(root, &path, depth + 1, out, total)?;
            } else if meta.is_file() {
                *total += meta.len();
                let relative = path
                    .strip_prefix(root)
                    .ok()
                    .and_then(|p| p.to_str())
                    .context("plugin file names must be UTF-8")?;
                out.push((relative.to_owned(), meta.permissions().mode(), meta.len()));
                if out.len() > MAX_FILES || *total > MAX_TREE_BYTES {
                    return refuse("plugin tree has too many files or is too large");
                }
            } else {
                return refuse(
                    "plugin trees may contain only regular files and directories (no symlinks)",
                );
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(root, root, 0, &mut out, &mut 0)?;
    out.sort();
    Ok(out)
}

/// Every declared resource is present with its digest, and nothing else is: an
/// undeclared executable or asset cannot ride along in a verified release. The
/// release's own `manifest.json` bytes must also match the digest the registry
/// recorded at install (`manifest_sha256`): the manifest declares the other digests, so
/// an edited manifest would otherwise vouch for edited files.
fn verify_tree(root: &Path, m: &Manifest, manifest_sha256: &str) -> Result<()> {
    let on_disk = read_regular(
        &root.join(manifest::MANIFEST_FILE),
        manifest::MAX_MANIFEST_BYTES as u64,
    )?;
    if manifest::digest_text(&on_disk) != manifest_sha256 {
        return refuse("manifest.json differs from the manifest recorded at install");
    }
    let present: BTreeSet<String> = walk(root)?.into_iter().map(|f| f.0).collect();
    let mut expected: BTreeSet<String> = m
        .resource_declarations
        .iter()
        .map(|r| r.path.clone())
        .collect();
    expected.insert(manifest::MANIFEST_FILE.to_owned());
    if let Some(extra) = present.difference(&expected).next() {
        return refuse(format!("undeclared file in plugin: {extra}"));
    }
    for resource in &m.resource_declarations {
        if !present.contains(&resource.path) {
            return refuse(format!(
                "declared resource {} is missing",
                resource.identity()
            ));
        }
        let bytes = read_regular(&root.join(&resource.path), MAX_FILE_BYTES)?;
        if manifest::digest_text(&bytes) != resource.digest {
            return refuse(format!("digest mismatch for {}", resource.identity()));
        }
    }
    Ok(())
}

struct Staged {
    dir: PathBuf,
    manifest: Manifest,
    manifest_sha256: String,
}

fn chmod(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Remove a (possibly read-only) release or staging tree. Symlinks are never followed.
fn remove_tree(path: &Path) -> std::io::Result<()> {
    fn open_up(path: &Path) {
        if let Ok(meta) = std::fs::symlink_metadata(path)
            && meta.is_dir()
        {
            let _ = chmod(path, 0o700);
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    open_up(&entry.path());
                }
            }
        }
    }
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    open_up(path);
    std::fs::remove_dir_all(path)
}

/// Copy the source tree into a private staging directory and verify it there. The staged
/// bytes, not the source, are authoritative from this point (no check/use race).
fn stage(paths: &Paths, source: &Path) -> Result<Staged> {
    if !source.is_absolute() {
        return refuse("plugin source must be an absolute directory path");
    }
    let root = source
        .canonicalize()
        .map_err(|_| PluginError("plugin source directory is not readable".into()))?;
    if !root.is_dir() {
        return refuse("plugin source must be a directory");
    }
    let early = Manifest::parse(&read_regular(
        &root.join(manifest::MANIFEST_FILE),
        manifest::MAX_MANIFEST_BYTES as u64,
    )?)?;
    safety::private_mkdir(&paths.plugins)?;
    let staging = paths.plugin_staging();
    safety::private_mkdir(&staging)?;
    let mut nonce = [0u8; 8];
    getrandom::getrandom(&mut nonce).map_err(|_| anyhow::anyhow!("random source unavailable"))?;
    let dir = staging.join(format!(
        "{}-{}",
        early.id,
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    safety::private_mkdir(&dir)?;
    let built = (|| -> Result<Staged> {
        let mut source_modes = BTreeMap::new();
        for (relative, mode, _) in walk(&root)? {
            let from = root.join(&relative);
            let to = dir.join(&relative);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
                chmod(parent, 0o700)?;
            }
            let bytes = read_regular(&from, MAX_FILE_BYTES)?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&to)?;
            std::io::Write::write_all(&mut file, &bytes)?;
            file.sync_all()?;
            source_modes.insert(relative, mode);
        }
        let manifest_bytes = read_regular(
            &dir.join(manifest::MANIFEST_FILE),
            manifest::MAX_MANIFEST_BYTES as u64,
        )?;
        let manifest = Manifest::parse(&manifest_bytes)?;
        if manifest.id != early.id {
            return refuse("manifest changed while it was being copied");
        }
        verify_tree(&dir, &manifest, &manifest::digest_text(&manifest_bytes))?;
        let script = manifest.executable();
        if source_modes
            .get(&script.path)
            .is_none_or(|m| m & 0o100 == 0)
        {
            return refuse("the runtime executable must have the owner execute bit set");
        }
        freeze(&dir, &script.path)?;
        Ok(Staged {
            dir: dir.clone(),
            manifest,
            manifest_sha256: manifest::digest_text(&manifest_bytes),
        })
    })();
    if built.is_err() {
        let _ = remove_tree(&dir);
    }
    built
}

/// Make a release read-only: files 0400 (the script 0500), subdirectories 0500. The
/// top directory is frozen separately after the atomic rename.
fn freeze(dir: &Path, script: &str) -> Result<()> {
    for (relative, _, _) in walk(dir)? {
        let executable = relative == script;
        chmod(&dir.join(&relative), if executable { 0o500 } else { 0o400 })?;
    }
    let mut directories = Vec::new();
    for (relative, _, _) in walk(dir)? {
        let mut parent = Path::new(&relative).parent();
        while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
            directories.push(dir.join(p));
            parent = p.parent();
        }
    }
    directories.sort();
    directories.dedup();
    for subdirectory in directories.iter().rev() {
        chmod(subdirectory, 0o500)?;
    }
    Ok(())
}

/// The manifest digest the registry recorded for each installed (id, version), read
/// from the admission store read-only. A home whose store has no plugin table has none.
fn recorded_manifest_digests(paths: &Paths) -> Result<BTreeMap<(String, String), String>> {
    let mut digests = BTreeMap::new();
    if !paths.dbos_admission.exists() {
        return Ok(digests);
    }
    let connection = rusqlite::Connection::open_with_flags(
        &paths.dbos_admission,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let Ok(mut statement) =
        connection.prepare("SELECT id,version,manifest_sha256 FROM forge_plugins")
    else {
        return Ok(digests);
    };
    for row in statement.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (id, version, sha) = row?;
        digests.insert((id, version), sha);
    }
    Ok(digests)
}

/// Verify every release directory against its own manifest (digests, no undeclared
/// files). With `freeze`, also restore the read-only modes (a restore copies files
/// with private writable modes). Used by backup (verify) and restore (verify + freeze).
pub fn verify_releases(paths: &Paths, freeze_after: bool) -> Result<usize> {
    let recorded = recorded_manifest_digests(paths)?;
    let mut count = 0;
    let Ok(plugins) = std::fs::read_dir(paths.plugin_releases()) else {
        return Ok(0);
    };
    for plugin in plugins {
        let plugin = plugin?;
        for version in std::fs::read_dir(plugin.path())? {
            let root = version?.path();
            let m = Manifest::parse(&read_regular(
                &root.join(manifest::MANIFEST_FILE),
                manifest::MAX_MANIFEST_BYTES as u64,
            )?)?;
            if root != paths.plugin_release(&m.id, &m.version) {
                return refuse("plugin release directory does not match its manifest identity");
            }
            let Some(sha) = recorded.get(&(m.id.clone(), m.version.clone())) else {
                return refuse(format!(
                    "plugin release {}/{} has no registry record",
                    m.id, m.version
                ));
            };
            verify_tree(&root, &m, sha).map_err(|e| {
                PluginError(format!(
                    "plugin release {}/{} fails verification: {e}",
                    m.id, m.version
                ))
            })?;
            if freeze_after {
                freeze(&root, &m.executable().path)?;
                chmod(&root, 0o500)?;
            }
            count += 1;
        }
    }
    Ok(count)
}

/// Atomically publish a staged tree as the immutable release. An existing directory at
/// the target can only be an orphan from a crash (the registry row is the authority and
/// the caller verified none exists), so it is replaced.
fn promote(staged: &Path, release: &Path) -> Result<()> {
    let parent = release.parent().context("release path has a parent")?;
    if let Some(grand) = parent.parent() {
        safety::private_mkdir(grand)?;
    }
    safety::private_mkdir(parent)?;
    remove_tree(release)?;
    std::fs::rename(staged, release)?;
    // The directory itself must stay writable until the rename (moving a directory
    // between parents updates its `..`); freeze it now.
    chmod(release, 0o500)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Startup recovery of the filesystem: nothing in staging survives a restart, and a
/// release directory with no registry row is an unfinished install.
fn reconcile_filesystem(paths: &Paths, known: &BTreeSet<(String, String)>) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths.plugin_staging()) {
        for entry in entries.flatten() {
            remove_tree(&entry.path())?;
            removed.push(format!("staging/{}", entry.file_name().to_string_lossy()));
        }
    }
    if let Ok(plugins) = std::fs::read_dir(paths.plugin_releases()) {
        for plugin in plugins.flatten() {
            let id = plugin.file_name().to_string_lossy().into_owned();
            if let Ok(versions) = std::fs::read_dir(plugin.path()) {
                for version in versions.flatten() {
                    let name = version.file_name().to_string_lossy().into_owned();
                    if !known.contains(&(id.clone(), name.clone())) {
                        remove_tree(&version.path())?;
                        removed.push(format!("releases/{id}/{name}"));
                    }
                }
            }
            let _ = std::fs::remove_dir(plugin.path());
        }
    }
    Ok(removed)
}

fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

fn cycle_through(candidate: &Manifest, installed: &BTreeMap<String, Entry>) -> bool {
    let mut seen = BTreeSet::new();
    let mut pending: Vec<&str> = candidate.requires.iter().map(String::as_str).collect();
    while let Some(id) = pending.pop() {
        if id == candidate.id {
            return true;
        }
        if seen.insert(id)
            && let Some(entry) = installed.get(id)
        {
            pending.extend(entry.manifest.requires.iter().map(String::as_str));
        }
    }
    false
}

fn content_type(path: &str) -> Option<&'static str> {
    match Path::new(path).extension()?.to_str()? {
        "js" | "mjs" => Some("text/javascript; charset=utf-8"),
        "css" => Some("text/css; charset=utf-8"),
        "json" => Some("application/json"),
        "svg" => Some("image/svg+xml"),
        "png" => Some("image/png"),
        "txt" => Some("text/plain; charset=utf-8"),
        "woff2" => Some("font/woff2"),
        _ => None,
    }
}

/// A response a plugin API route produced, already validated against the allow-lists.
pub struct ApiReply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

fn work_resource_path(m: &Manifest, kind: Kind, id: &str) -> Result<String> {
    let want = if kind == Kind::Job {
        ResourceKind::Job
    } else {
        ResourceKind::Workflow
    };
    Ok(m.resource(&format!("{}:{id}", want.name()))?.path.clone())
}

struct Planned {
    work_id: String,
    contribution_id: String,
    kind: Kind,
    raw: String,
    resource: WorkResource,
}

impl Core {
    // ---- Dispatch gates -------------------------------------------------------------
    /// The executable behind a `plugin.NAME` work type: a configured plugin, or an
    /// installed plugin that is currently enabled. A disabled, broken or draining
    /// plugin yields `None`, so it cannot dispatch anywhere.
    pub fn plugin(&self, kind: &str) -> Option<Plugin> {
        if let Some(plugin) = self.plugins.get(kind) {
            return Some(plugin.clone());
        }
        let id = kind.strip_prefix("plugin.")?;
        let state = self.registry.state.read().ok()?;
        let entry = state.entries.get(id)?;
        if entry.phase == Phase::Enabled {
            entry.plugin.clone()
        } else {
            None
        }
    }
    pub(crate) fn installed_types(&self) -> Vec<(String, Plugin)> {
        let Ok(state) = self.registry.state.read() else {
            return Vec::new();
        };
        state
            .entries
            .iter()
            .filter(|(_, e)| e.phase == Phase::Enabled)
            .filter_map(|(id, e)| Some((format!("plugin.{id}"), e.plugin.clone()?)))
            .collect()
    }
    /// Work materialized from a plugin dispatches only while that plugin is enabled.
    /// Enforced at admission and again before every dispatch (`Core::admitted`).
    pub(crate) fn plugin_work_gate(&self, work_id: &str) -> Result<()> {
        let state = self
            .registry
            .state
            .read()
            .map_err(|_| Denied("plugin registry unavailable"))?;
        let Some(owner) = state.owned_work.get(work_id) else {
            return Ok(());
        };
        match state.entries.get(owner).map(|e| e.phase) {
            Some(Phase::Enabled) => Ok(()),
            _ => Err(Denied("work belongs to a plugin that is not enabled").into()),
        }
    }
    fn begin_call(&self, id: &str) -> Result<(Plugin, Manifest, CallGuard)> {
        let state = self
            .registry
            .state
            .read()
            .map_err(|_| Denied("plugin registry unavailable"))?;
        let entry = state
            .entries
            .get(id)
            .filter(|e| e.phase == Phase::Enabled)
            .ok_or(PluginError("plugin is not installed and enabled".into()))?;
        let plugin = entry.plugin.clone().context("plugin has no executable")?;
        let manifest = entry.manifest.clone();
        // Counted while the state lock is held, so a disable that flips the phase
        // afterwards always observes this call.
        *self
            .registry
            .inflight
            .lock()
            .map_err(|_| Denied("plugin registry unavailable"))?
            .entry(id.to_owned())
            .or_default() += 1;
        Ok((
            plugin,
            manifest,
            CallGuard {
                registry: self.registry.clone(),
                id: id.to_owned(),
            },
        ))
    }

    #[cfg(test)]
    pub(crate) fn hold_call_for_test(&self, id: &str) -> CallGuard {
        self.begin_call(id).expect("plugin enabled").2
    }

    // ---- Startup --------------------------------------------------------------------
    /// Load the registry, remove unfinished installs, and activate enabled plugins.
    /// Runs after the DBOS client exists so deactivation can pause schedules.
    pub async fn plugins_startup(&self) -> Result<()> {
        let guard = self.registry.lock_lifecycle().await;
        let rows = self.store.write(|c| {
            let mut statement = c.prepare(
                "SELECT id,version,state,manifest,manifest_sha256,installed FROM forge_plugins ORDER BY id",
            )?;
            Ok(statement
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, f64>(5)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        let known = rows.iter().map(|r| (r.0.clone(), r.1.clone())).collect();
        let paths = self.paths.clone();
        let (removed, _guard) = tokio::task::spawn_blocking(move || {
            Ok::<_, anyhow::Error>((reconcile_filesystem(&paths, &known)?, guard))
        })
        .await??;
        for item in removed {
            self.record(
                "runtime",
                "plugin_cleanup",
                &item,
                "unfinished install removed",
            )?;
        }
        let owned: Vec<(String, String)> = self.store.write(|c| {
            let mut statement =
                c.prepare("SELECT work_id,plugin_id FROM forge_plugin_resources")?;
            Ok(statement
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        {
            let mut state = self
                .registry
                .state
                .write()
                .map_err(|_| anyhow::anyhow!("poisoned"))?;
            state.owned_work = owned.into_iter().collect();
        }
        for (id, version, db_state, raw, sha, installed) in rows {
            let parsed = Manifest::parse(raw.as_bytes());
            let release = absolute(self.paths.plugin_release(&id, &version));
            let mut entry = match parsed {
                Ok(manifest) if manifest.id == id && manifest.version == version => Entry {
                    manifest,
                    manifest_sha256: sha,
                    phase: Phase::Installed,
                    plugin: None,
                    problem: None,
                    installed,
                },
                _ => {
                    self.record("runtime", "plugin_broken", &id, "stored manifest invalid")?;
                    continue;
                }
            };
            let verified = {
                let (release, manifest, sha) = (
                    release.clone(),
                    entry.manifest.clone(),
                    entry.manifest_sha256.clone(),
                );
                tokio::task::spawn_blocking(move || verify_tree(&release, &manifest, &sha)).await?
            };
            match (&verified, db_state.as_str()) {
                (Err(error), _) => {
                    entry.phase = Phase::Broken;
                    entry.problem = Some(error.to_string());
                }
                (Ok(()), "enabled") => match self.describe(&entry.manifest, &release).await {
                    Ok(plugin) => {
                        entry.phase = Phase::Enabled;
                        entry.plugin = Some(plugin);
                    }
                    Err(error) => {
                        entry.phase = Phase::Broken;
                        entry.problem = Some(format!("plugin failed to describe itself: {error}"));
                    }
                },
                _ => {}
            }
            let deactivate = entry.phase != Phase::Enabled;
            self.registry
                .state
                .write()
                .map_err(|_| anyhow::anyhow!("poisoned"))?
                .entries
                .insert(id.clone(), entry);
            if deactivate {
                // A plugin that is not enabled never leaves live jobs behind (also repairs a
                // crash between a state change and its work/schedule changes).
                self.deactivate_materialized(&id).await?;
            }
        }
        Ok(())
    }

    async fn describe(&self, m: &Manifest, release: &Path) -> Result<Plugin> {
        let script = m.executable();
        let config = PluginConfig {
            command: vec![release.join(&script.path).to_string_lossy().into_owned()],
            timeout_seconds: m.runtime.timeout_seconds,
            env_passthrough: vec![],
        };
        let reply = plugin::invoke(
            &config,
            json!({"api_version":manifest::PROTOCOL,"operation":"describe"}),
        )
        .await?;
        let description = reply["description"]
            .as_str()
            .filter(|d| !d.is_empty() && d.len() <= 1024)
            .context("plugin description missing")?
            .to_owned();
        let schema = reply["schema"].clone();
        if !schema.is_object() || jsonschema::validator_for(&schema).is_err() {
            anyhow::bail!("plugin schema missing or invalid");
        }
        Ok(Plugin {
            config,
            description,
            schema,
            api_version: manifest::PROTOCOL,
        })
    }

    // ---- Views ----------------------------------------------------------------------
    fn entry_view(&self, id: &str, e: &Entry) -> Value {
        let m = &e.manifest;
        let enabled = e.phase == Phase::Enabled;
        let asset = |reference: &str| -> Option<String> {
            let resource = m.resource(reference).ok()?;
            Some(format!("/plugins/{id}/assets/{}", resource.path))
        };
        let dashboard: Vec<Value> = m
            .dashboard()
            .into_iter()
            .map(|r| {
                json!({"path":r.path,"label":r.label,"entry":asset(&r.entry),
                    "stylesheet":r.stylesheet.as_deref().and_then(asset)})
            })
            .collect();
        json!({
            "id":id,"version":m.version,"state":e.phase.label(),"enabled":enabled,
            "description":m.description,"problem":e.problem,"installed":e.installed,
            "type":m.work_type(),"manifest_sha256":e.manifest_sha256,
            "requires":m.requires,"conflicts":m.conflicts,"capabilities":m.capabilities,
            "contributions":m.contributions.iter().map(|c| json!({"kind":c.kind.name(),"id":c.id})).collect::<Vec<_>>(),
            "cli":m.cli().iter().map(|c| json!({"command_path":c.command_path,"description":c.description})).collect::<Vec<_>>(),
            "api":m.api().iter().flat_map(|a| a.routes.iter().map(|r| json!({"method":r.method,"path":r.path,"capability":r.capability}))).collect::<Vec<_>>(),
            "mcp":m.mcp().iter().map(|t| m.mcp_name(&t.tool_name)).collect::<Vec<_>>(),
            "dashboard":dashboard,
            "work":m.of_kind(Kind::Job).chain(m.of_kind(Kind::Workflow)).map(|c| m.work_id(&c.id)).collect::<Vec<_>>(),
        })
    }
    /// Operator view of every installed plugin (also embedded in the snapshot).
    pub fn plugins_view(&self) -> Value {
        let Ok(state) = self.registry.state.read() else {
            return json!([]);
        };
        Value::Array(
            state
                .entries
                .iter()
                .map(|(id, e)| self.entry_view(id, e))
                .collect(),
        )
    }
    pub fn plugins_list(&self, actor: &str) -> Result<Value> {
        self.permit(actor, "read", None)?;
        Ok(self.plugins_view())
    }
    /// One installed plugin, optionally re-verifying its release and re-describing it.
    pub async fn plugin_show(&self, actor: &str, id: &str, check: bool) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let (view, manifest) = {
            let state = self
                .registry
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("poisoned"))?;
            let entry = state
                .entries
                .get(id)
                .ok_or(PluginError("plugin is not installed".into()))?;
            (
                self.entry_view(id, entry),
                (entry.manifest.clone(), entry.manifest_sha256.clone()),
            )
        };
        let (manifest, recorded) = manifest;
        if !check {
            return Ok(view);
        }
        let release = absolute(self.paths.plugin_release(id, &manifest.version));
        let verified = {
            let (release, manifest, recorded) =
                (release.clone(), manifest.clone(), recorded.clone());
            tokio::task::spawn_blocking(move || verify_tree(&release, &manifest, &recorded)).await?
        };
        let mut health = json!({"release_verified":verified.is_ok(),"error":verified.as_ref().err().map(|e| e.to_string())});
        if verified.is_ok() && view["enabled"] == true {
            let described = self.describe(&manifest, &release).await;
            health["describe_ok"] = json!(described.is_ok());
            if let Err(error) = described {
                health["error"] = json!(error.to_string());
            }
        }
        let mut view = view;
        view["health"] = health;
        Ok(view)
    }

    // ---- Install --------------------------------------------------------------------
    pub async fn plugin_install(&self, actor: &str, source: &str, dry_run: bool) -> Result<Value> {
        self.plugin_install_with(actor, source, dry_run, None).await
    }
    /// The `plugins` capability for a lifecycle operation. A denied attempt is audited
    /// (actor, operation, target), because a refused lifecycle change is security-relevant.
    fn lifecycle_permit(&self, actor: &str, operation: &str, target: &str) -> Result<()> {
        if let Err(error) = self.permit(actor, "plugins", None) {
            let _ = self.record(actor, "plugin_denied", target, operation);
            return Err(error);
        }
        Ok(())
    }
    /// Work types a plugin's jobs and workflows run, with whether this actor may
    /// register and execute each one. A plugin whose own type is not granted installs,
    /// but cannot be enabled until the operator adds the type to the actor's `types`.
    fn work_authority(&self, actor: &str, planned: &[Planned]) -> (Vec<Value>, Vec<String>) {
        let mut types: BTreeSet<String> = BTreeSet::new();
        let rows = planned
            .iter()
            .map(|p| {
                let kind = p.resource.work["type"].as_str().unwrap_or("").to_owned();
                types.insert(kind.clone());
                json!({"work_id":p.work_id,"kind":if p.kind == Kind::Job {"job"} else {"workflow"},"type":kind,"scheduled":p.resource.schedule.is_some(),"enabled":p.resource.enabled})
            })
            .collect();
        let gaps = types
            .into_iter()
            .filter(|t| {
                self.permit(actor, "register", Some(t)).is_err()
                    || self.permit(actor, "execute", Some(t)).is_err()
            })
            .collect();
        (rows, gaps)
    }
    /// Strict, install-time validation of one job/workflow resource: its shape, the
    /// actor's authority and, for core types, the complete registration checks. Types
    /// owned by plugins cannot be described before they are enabled, so a definition that
    /// runs one is validated structurally here and completely at enable.
    fn install_check(&self, actor: &str, p: &Planned) -> Result<()> {
        let work = &p.resource.work;
        let shape = work.as_object().is_some_and(|o| {
            o.len() == 2
                && o.get("type").is_some_and(Value::is_string)
                && o.get("input").is_some_and(Value::is_object)
        });
        if !shape {
            return refuse(format!(
                "resource {} must have exactly a string `type` and an object `input`",
                p.work_id
            ));
        }
        if work["type"]
            .as_str()
            .is_some_and(|t| t.starts_with("plugin."))
        {
            return Ok(());
        }
        // Closed structural validation of flows (dependencies, references, limits) needs
        // no type catalog, so it is complete even when a step runs a not-yet-enabled
        // plugin type.
        if matches!(work["type"].as_str(), Some("workflow" | "agent")) {
            crate::flow_schema::validate(work).map_err(|error| {
                PluginError(format!("resource {} is invalid: {error:#}", p.work_id))
            })?;
        }
        match self.check_definition(actor, work) {
            Ok(()) => Ok(()),
            Err(error)
                if matches!(
                    error.to_string().as_str(),
                    "unknown work type" | "unsupported action type"
                ) && work.to_string().contains("\"plugin.") =>
            {
                Ok(())
            }
            // Missing authority is reported as an authority gap (and refused at enable,
            // naming the type) rather than failing the install of an otherwise valid release.
            Err(error) if error.is::<crate::core::Denied>() => Ok(()),
            Err(error) => refuse(format!("resource {} is invalid: {error}", p.work_id)),
        }
    }
    pub(crate) async fn plugin_install_with(
        &self,
        actor: &str,
        source: &str,
        dry_run: bool,
        crash: Option<CrashAt>,
    ) -> Result<Value> {
        self.lifecycle_permit(actor, "install", source_label(source))?;
        let guard = self.registry.lock_lifecycle().await;
        let paths = self.paths.clone();
        let from = PathBuf::from(source);
        let staged = tokio::task::spawn_blocking(move || stage(&paths, &from)).await??;
        crash_point(CrashAt::AfterStage, crash)?;
        let checked = self.check_install(&staged.manifest).and_then(|()| {
            let planned = self.plan_work(actor, &staged.manifest, &staged.dir)?;
            for p in &planned {
                self.install_check(actor, p)?;
            }
            Ok(planned)
        });
        if checked.is_err() || dry_run {
            let _ = remove_tree(&staged.dir);
            let planned = checked?;
            let (work, gaps) = self.work_authority(actor, &planned);
            let hint = gaps
                .iter()
                .map(|t| {
                    format!("add \"{t}\" to actor {actor}'s `types` in config.json before enabling")
                })
                .collect::<Vec<_>>();
            return Ok(
                json!({"valid":true,"installed":false,"id":staged.manifest.id,"version":staged.manifest.version,
                    "work":work,"work_types":planned.iter().filter_map(|p| p.resource.work["type"].as_str().map(str::to_owned)).collect::<BTreeSet<_>>(),
                    "authority_gaps":gaps,"hints":hint}),
            );
        }
        let planned_view = checked.map(|p| self.work_authority(actor, &p))?;
        let m = staged.manifest.clone();
        let release = absolute(self.paths.plugin_release(&m.id, &m.version));
        let _guard = {
            let (dir, release) = (staged.dir.clone(), release.clone());
            // Promotion can outlive a dropped request future. Keep lifecycle
            // ownership with the worker until it returns the guard for registry insertion.
            tokio::task::spawn_blocking(move || {
                promote(&dir, &release)?;
                Ok::<_, anyhow::Error>(guard)
            })
            .await??
        };
        crash_point(CrashAt::AfterRename, crash)?;
        let raw = String::from_utf8(read_regular(
            &release.join(manifest::MANIFEST_FILE),
            manifest::MAX_MANIFEST_BYTES as u64,
        )?)
        .context("manifest is UTF-8")?;
        let installed = chrono::Utc::now().timestamp_millis() as f64 / 1000.;
        let inserted = self.store.write(|c| {
            c.execute(
                "INSERT INTO forge_plugins(id,version,state,manifest,manifest_sha256,installed) VALUES(?1,?2,'installed',?3,?4,?5)",
                rusqlite::params![m.id, m.version, raw, staged.manifest_sha256, installed],
            )?;
            Ok(())
        });
        if let Err(error) = inserted {
            let _ = remove_tree(&release);
            return Err(error.into());
        }
        let entry = Entry {
            manifest: m.clone(),
            manifest_sha256: staged.manifest_sha256,
            phase: Phase::Installed,
            plugin: None,
            problem: None,
            installed,
        };
        let view = self.entry_view(&m.id, &entry);
        self.registry
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .entries
            .insert(m.id.clone(), entry);
        self.record(actor, "plugin_install", &m.id, &m.version)?;
        let mut view = view;
        view["work"] = json!(planned_view.0);
        view["authority_gaps"] = json!(planned_view.1);
        Ok(view)
    }

    fn check_install(&self, m: &Manifest) -> Result<()> {
        let state = self
            .registry
            .state
            .read()
            .map_err(|_| anyhow::anyhow!("poisoned"))?;
        if state.entries.contains_key(&m.id) {
            return refuse(format!(
                "plugin {} is already installed; upgrade is not supported (uninstall it first)",
                m.id
            ));
        }
        if self.policy.plugins.contains_key(&m.work_type()) {
            return refuse(format!(
                "ownership collision: a configured plugin type {} already exists",
                m.work_type()
            ));
        }
        let mut owned = BTreeSet::new();
        for entry in state.entries.values() {
            owned.extend(entry.manifest.ownership());
        }
        if let Some(key) = m.ownership().into_iter().find(|k| owned.contains(k)) {
            return refuse(format!("ownership collision: {key}"));
        }
        if cycle_through(m, &state.entries) {
            return refuse(format!("dependency cycle through {}", m.id));
        }
        for c in m.of_kind(Kind::Job).chain(m.of_kind(Kind::Workflow)) {
            let work_id = m.work_id(&c.id);
            let existing: Option<Option<String>> = self.store.write(|db| {
                use rusqlite::OptionalExtension;
                Ok(db
                    .query_row(
                        "SELECT (SELECT plugin_id FROM forge_plugin_resources WHERE work_id=?1) FROM forge_work WHERE id=?1",
                        [&work_id],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .optional()?)
            })?;
            // Work left by an earlier install of this same plugin is adopted on enable.
            if let Some(owner) = existing
                && owner.as_deref() != Some(m.id.as_str())
            {
                return refuse(format!(
                    "ownership collision: work id {work_id} already exists"
                ));
            }
        }
        Ok(())
    }

    // ---- Enable ---------------------------------------------------------------------
    pub async fn plugin_enable(&self, actor: &str, id: &str) -> Result<Value> {
        self.lifecycle_permit(actor, "enable", id)?;
        let _guard = self.registry.lock_lifecycle().await;
        let manifest = {
            let state = self
                .registry
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("poisoned"))?;
            let entry = state
                .entries
                .get(id)
                .ok_or(PluginError("plugin is not installed".into()))?;
            match entry.phase {
                Phase::Installed => {}
                Phase::Enabled => return refuse("plugin is already enabled"),
                Phase::Disabling => return refuse("plugin is being disabled"),
                Phase::Broken => {
                    return refuse(format!(
                        "plugin cannot be enabled: {}",
                        entry.problem.as_deref().unwrap_or("release is broken")
                    ));
                }
            }
            for required in &entry.manifest.requires {
                if state.entries.get(required).map(|e| e.phase) != Some(Phase::Enabled) {
                    return refuse(format!("required plugin {required} is not enabled"));
                }
            }
            for other in state.entries.values().filter(|e| e.phase == Phase::Enabled) {
                if entry.manifest.conflicts.contains(&other.manifest.id)
                    || other.manifest.conflicts.contains(&entry.manifest.id)
                {
                    return refuse(format!(
                        "plugin conflict with enabled plugin {}",
                        other.manifest.id
                    ));
                }
            }
            (entry.manifest.clone(), entry.manifest_sha256.clone())
        };
        let (manifest, recorded) = manifest;
        if self.policy.plugins.contains_key(&manifest.work_type()) {
            return refuse("a configured plugin type with the same name exists");
        }
        let release = absolute(self.paths.plugin_release(id, &manifest.version));
        // Re-verify the exact bytes that will run, and the recorded manifest identity.
        {
            let (release, m, recorded) = (release.clone(), manifest.clone(), recorded.clone());
            tokio::task::spawn_blocking(move || verify_tree(&release, &m, &recorded)).await??;
        }
        let described = self.describe(&manifest, &release).await?;
        let planned = self.plan_work(actor, &manifest, &release)?;
        // Name the exact missing type instead of a generic authority refusal.
        let (_, gaps) = self.work_authority(actor, &planned);
        if let Some(gap) = gaps.first() {
            return refuse(format!(
                "actor {actor} may not register and execute work of type {gap}: add \"{gap}\" to the actor's `types` (with the register and execute capabilities) in config.json, restart, then enable plugin {id}"
            ));
        }
        self.set_entry(id, Phase::Enabled, Some(described), None)?;
        if let Err(error) = self.activate(actor, &manifest, &planned).await {
            let _ = self.deactivate_materialized(id).await;
            self.set_entry(id, Phase::Installed, None, None)?;
            return Err(error);
        }
        self.store.write(|c| {
            c.execute("UPDATE forge_plugins SET state='enabled' WHERE id=?1", [id])?;
            Ok(())
        })?;
        self.record(actor, "plugin_enable", id, &manifest.version)?;
        let state = self
            .registry
            .state
            .read()
            .map_err(|_| anyhow::anyhow!("poisoned"))?;
        Ok(self.entry_view(id, &state.entries[id]))
    }

    fn set_entry(
        &self,
        id: &str,
        phase: Phase,
        plugin: Option<Plugin>,
        problem: Option<String>,
    ) -> Result<()> {
        let mut state = self
            .registry
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("poisoned"))?;
        let entry = state.entries.get_mut(id).context("plugin vanished")?;
        entry.phase = phase;
        entry.plugin = plugin;
        entry.problem = problem;
        Ok(())
    }

    fn plan_work(&self, actor: &str, m: &Manifest, release: &Path) -> Result<Vec<Planned>> {
        let mut planned = Vec::new();
        for c in m.of_kind(Kind::Job).chain(m.of_kind(Kind::Workflow)) {
            let path = work_resource_path(m, c.kind, &c.id)?;
            let resource =
                manifest::parse_work_resource(&read_regular(&release.join(path), MAX_FILE_BYTES)?)?;
            if let Some(schedule) = &resource.schedule {
                crate::core::check_schedule_timing(&schedule.cron, &schedule.timezone)?;
            }
            let _ = actor;
            planned.push(Planned {
                work_id: m.work_id(&c.id),
                contribution_id: c.id.clone(),
                kind: c.kind,
                raw: crate::canonical::to_string(&resource.work)?,
                resource,
            });
        }
        Ok(planned)
    }

    /// Materialize jobs/workflows into the core work store and schedules.
    async fn activate(&self, actor: &str, m: &Manifest, planned: &[Planned]) -> Result<()> {
        for p in planned {
            self.check_definition(actor, &p.resource.work)?;
        }
        for p in planned {
            // Always inserted disabled first; the declared state is applied last.
            self.store.write(|c| {
                use rusqlite::OptionalExtension;
                let old = c
                    .query_row(
                        "SELECT owner,definition FROM forge_work WHERE id=?1",
                        [&p.work_id],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                    )
                    .optional()?;
                match old {
                    Some((owner, definition)) => {
                        if owner != actor || definition != p.raw {
                            return Err(crate::dbos::admission::Error::Invalid(
                                "plugin work already exists with a different owner or definition",
                            ));
                        }
                        c.execute("UPDATE forge_work SET enabled=0 WHERE id=?1", [&p.work_id])?;
                    }
                    None => {
                        c.execute(
                            "INSERT INTO forge_work VALUES(?1,?2,?3,0,?4)",
                            rusqlite::params![
                                p.work_id,
                                actor,
                                p.raw,
                                chrono::Utc::now().timestamp_millis() as f64 / 1000.
                            ],
                        )?;
                    }
                }
                c.execute(
                    "INSERT INTO forge_plugin_resources(work_id,plugin_id,kind,resource_id,schedule_name) VALUES(?1,?2,?3,?4,NULL)
                     ON CONFLICT(work_id) DO UPDATE SET plugin_id=excluded.plugin_id",
                    rusqlite::params![p.work_id, m.id, p.kind.name(), p.contribution_id],
                )?;
                Ok(())
            })?;
            self.registry
                .state
                .write()
                .map_err(|_| anyhow::anyhow!("poisoned"))?
                .owned_work
                .insert(p.work_id.clone(), m.id.clone());
            if let Some(schedule) = &p.resource.schedule {
                let name = self.materialize_schedule(actor, p, schedule).await?;
                self.store.write(|c| {
                    c.execute(
                        "UPDATE forge_plugin_resources SET schedule_name=?2 WHERE work_id=?1",
                        rusqlite::params![p.work_id, name],
                    )?;
                    Ok(())
                })?;
            }
            // The declared state: `enabled:false` jobs (and their paused schedules) stay off.
            if p.resource.enabled {
                self.store.write(|c| {
                    c.execute("UPDATE forge_work SET enabled=1 WHERE id=?1", [&p.work_id])?;
                    Ok(())
                })?;
            }
            self.record(
                actor,
                "plugin_work",
                &p.work_id,
                if p.resource.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
            )?;
        }
        Ok(())
    }

    async fn materialize_schedule(
        &self,
        actor: &str,
        p: &Planned,
        schedule: &manifest::WorkSchedule,
    ) -> Result<String> {
        let short = p.work_id.clone();
        let full = format!("{actor}.{short}");
        let _edit = self.lock_schedule(&full).await?;
        self.require_schedule_settled(&full)?;
        let existing = self.native()?.get_schedule(&full).await?;
        let name = match existing {
            Some(found) => {
                // A retained schedule from an earlier enable: reuse it, never redefine it.
                let context = found.decode_context::<crate::core::ScheduleContext>()?;
                if context.work_id != p.work_id || context.actor != actor {
                    return refuse("schedule name is retained for a different intent");
                }
                full
            }
            None => {
                self.create_schedule(
                    actor,
                    &short,
                    &p.work_id,
                    &schedule.cron,
                    &schedule.timezone,
                    None,
                )
                .await?
            }
        };
        // Declared-disabled jobs keep their schedule paused; enabled ones fire.
        if p.resource.enabled {
            self.native()?.resume_schedule(&name).await?;
        } else {
            self.native()?.pause_schedule(&name).await?;
        }
        Ok(name)
    }

    /// Disable every work item and pause every schedule this plugin materialized.
    /// Idempotent; the rows stay so run history remains inspectable.
    async fn deactivate_materialized(&self, id: &str) -> Result<()> {
        let rows: Vec<(String, Option<String>)> = self.store.write(|c| {
            let mut statement = c.prepare(
                "SELECT work_id,schedule_name FROM forge_plugin_resources WHERE plugin_id=?1",
            )?;
            Ok(statement
                .query_map([id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        for (work_id, schedule) in rows {
            self.store.write(|c| {
                c.execute("UPDATE forge_work SET enabled=0 WHERE id=?1", [&work_id])?;
                Ok(())
            })?;
            if let (Some(name), Some(client)) = (schedule, self.client.get()) {
                let _edit = self.lock_schedule(&name).await?;
                if client.get_schedule(&name).await?.is_some() {
                    client.pause_schedule(&name).await?;
                }
            }
        }
        Ok(())
    }

    // ---- Disable / uninstall --------------------------------------------------------
    /// Work and runs that reference this plugin: its own materialized work plus any
    /// other work whose definition names its type or one of its work ids.
    fn referencing_work(&self, m: &Manifest) -> Result<(Vec<String>, Vec<String>)> {
        let own: BTreeSet<String> = m
            .of_kind(Kind::Job)
            .chain(m.of_kind(Kind::Workflow))
            .map(|c| m.work_id(&c.id))
            .collect();
        let quoted_type = format!("\"{}\"", m.work_type());
        let rows: Vec<(String, String)> = self.store.write(|c| {
            let mut statement = c.prepare("SELECT id,definition FROM forge_work")?;
            Ok(statement
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        let mut mine = Vec::new();
        let mut others = Vec::new();
        for (work_id, definition) in rows {
            if own.contains(&work_id) {
                mine.push(work_id);
            } else if definition.contains(&quoted_type)
                || own
                    .iter()
                    .any(|w| definition.contains(&format!("\"work_id\":\"{w}\"")))
            {
                others.push(work_id);
            }
        }
        Ok((mine, others))
    }

    async fn active_runs(&self, work_ids: &[String]) -> Result<usize> {
        let mut active = 0;
        for work_id in work_ids {
            let ids: Vec<String> = self.store.write(|c| {
                let mut statement = c.prepare(
                    "SELECT dbos_id FROM dbos_invocations WHERE json_extract(envelope,'$.invocation.workspace')=?1 ORDER BY rowid DESC LIMIT ?2",
                )?;
                Ok(statement
                    .query_map(rusqlite::params![work_id, SCAN_RUNS], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?)
            })?;
            for id in ids {
                let rows = self
                    .native()?
                    .list_workflows(&dbos::sysdb::types::WorkflowFilter {
                        workflow_ids: vec![&id],
                        load_input: false,
                        load_output: false,
                        ..Default::default()
                    })
                    .await?;
                let status = rows
                    .first()
                    .map(|r| r.status.as_str())
                    .unwrap_or("ADMITTED");
                if ACTIVE.contains(&status) {
                    active += 1;
                }
            }
        }
        Ok(active)
    }

    pub async fn plugin_disable(&self, actor: &str, id: &str) -> Result<Value> {
        self.lifecycle_permit(actor, "disable", id)?;
        let _guard = self.registry.lock_lifecycle().await;
        let manifest =
            {
                let state = self
                    .registry
                    .state
                    .read()
                    .map_err(|_| anyhow::anyhow!("poisoned"))?;
                let entry = state
                    .entries
                    .get(id)
                    .ok_or(PluginError("plugin is not installed".into()))?;
                if entry.phase != Phase::Enabled {
                    return refuse("plugin is not enabled");
                }
                if let Some(dependent) = state.entries.values().find(|e| {
                    e.phase == Phase::Enabled && e.manifest.requires.iter().any(|r| r == id)
                }) {
                    return refuse(format!(
                        "plugin {} requires {id}; disable it first",
                        dependent.manifest.id
                    ));
                }
                entry.manifest.clone()
            };
        // Block new dispatch first, then look for activity; restore on refusal.
        let previous = {
            let mut state = self
                .registry
                .state
                .write()
                .map_err(|_| anyhow::anyhow!("poisoned"))?;
            let entry = state.entries.get_mut(id).context("plugin vanished")?;
            let plugin = entry.plugin.clone();
            entry.phase = Phase::Disabling;
            plugin
        };
        let restore = |this: &Self| this.set_entry(id, Phase::Enabled, previous.clone(), None);
        let inflight = self
            .registry
            .inflight
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .get(id)
            .copied()
            .unwrap_or(0);
        if inflight > 0 {
            restore(self)?;
            return refuse(format!(
                "plugin has {inflight} CLI/API/MCP call(s) in flight"
            ));
        }
        let (mine, others) = self.referencing_work(&manifest)?;
        let mut all = mine;
        all.extend(others);
        let active = match self.active_runs(&all).await {
            Ok(n) => n,
            Err(error) => {
                restore(self)?;
                return Err(error);
            }
        };
        if active > 0 {
            restore(self)?;
            return refuse(format!(
                "plugin has {active} active run(s); wait or cancel them first"
            ));
        }
        self.store.write(|c| {
            c.execute(
                "UPDATE forge_plugins SET state='installed' WHERE id=?1",
                [id],
            )?;
            Ok(())
        })?;
        self.set_entry(id, Phase::Installed, None, None)?;
        self.deactivate_materialized(id).await?;
        self.record(actor, "plugin_disable", id, &manifest.version)?;
        let state = self
            .registry
            .state
            .read()
            .map_err(|_| anyhow::anyhow!("poisoned"))?;
        Ok(self.entry_view(id, &state.entries[id]))
    }

    pub async fn plugin_uninstall(&self, actor: &str, id: &str) -> Result<Value> {
        self.lifecycle_permit(actor, "uninstall", id)?;
        let guard = self.registry.lock_lifecycle().await;
        let manifest = {
            let state = self
                .registry
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("poisoned"))?;
            let entry = state
                .entries
                .get(id)
                .ok_or(PluginError("plugin is not installed".into()))?;
            if entry.phase == Phase::Enabled || entry.phase == Phase::Disabling {
                return refuse("disable the plugin before uninstalling it");
            }
            if let Some(dependent) = state
                .entries
                .values()
                .find(|e| e.manifest.requires.iter().any(|r| r == id))
            {
                return refuse(format!("plugin {} requires {id}", dependent.manifest.id));
            }
            entry.manifest.clone()
        };
        let (_, others) = self.referencing_work(&manifest)?;
        if !others.is_empty() {
            return refuse(format!(
                "work still references this plugin: {}",
                others.join(", ")
            ));
        }
        // The registry row goes first: from this commit the plugin is gone, and the
        // release directory below is just an orphan if we crash before removing it.
        self.store.write(|c| {
            c.execute("DELETE FROM forge_plugins WHERE id=?1", [id])?;
            Ok(())
        })?;
        self.registry
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .entries
            .remove(id);
        let release = absolute(self.paths.plugin_release(id, &manifest.version));
        let (removed, _guard) = tokio::task::spawn_blocking(move || {
            let removed = remove_tree(&release);
            if removed.is_ok()
                && let Some(parent) = release.parent()
            {
                let _ = std::fs::remove_dir(parent);
            }
            (removed, guard)
        })
        .await?;
        if let Err(error) = removed {
            self.record("runtime", "plugin_cleanup_failed", id, &error.to_string())?;
        }
        self.record(actor, "plugin_uninstall", id, &manifest.version)?;
        Ok(
            json!({"id":id,"uninstalled":true,"retained":"materialized work and run history stay readable but cannot dispatch"}),
        )
    }

    // ---- CLI ------------------------------------------------------------------------
    pub async fn plugin_run(&self, actor: &str, id: &str, command: &[String]) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let (declared, phase, manifest) = {
            let state = self
                .registry
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("poisoned"))?;
            let entry = state
                .entries
                .get(id)
                .ok_or(PluginError("plugin is not installed".into()))?;
            (entry.manifest.cli(), entry.phase, entry.manifest.clone())
        };
        // Only a manifest-declared command path (longest prefix) is ever run.
        let matched = declared
            .iter()
            .filter(|c| {
                command.len() >= c.command_path.len()
                    && command[..c.command_path.len()] == c.command_path[..]
            })
            .max_by_key(|c| c.command_path.len())
            .ok_or(PluginError("command is not declared by this plugin".into()))?;
        // Authority first, so an unauthorized actor never sees the disabled-state denial.
        self.permit(actor, "execute", Some(&manifest.work_type()))?;
        if phase != Phase::Enabled {
            let denial = matched
                .disabled_denial
                .clone()
                .unwrap_or_else(|| serde_json::from_value(json!({})).expect("denial defaults"));
            self.record(
                actor,
                "plugin_cli_denied",
                id,
                &matched.command_path.join(" "),
            )?;
            return Ok(json!({"denied":true,"status":denial.status,"message":denial.message}));
        }
        let args = &command[matched.command_path.len()..];
        if args.len() > 64 || args.iter().any(|a| a.len() > 4096 || a.contains('\0')) {
            return Err(Invalid("too many or oversized command arguments").into());
        }
        let (plugin, _, _guard) = self.begin_call(id)?;
        self.record(actor, "plugin_cli", id, &matched.command_path.join(" "))?;
        let reply = plugin::invoke(
            &plugin.config,
            json!({"api_version":manifest::PROTOCOL,"operation":"cli","command":matched.command_path,"args":args,"principal":actor}),
        )
        .await?;
        let exit = reply["exit_code"].as_u64().filter(|c| *c <= 255);
        let stdout = reply["stdout"].as_str().unwrap_or("");
        let stderr = reply["stderr"].as_str().unwrap_or("");
        if exit.is_none() || stdout.len() > MAX_CLI_OUTPUT || stderr.len() > MAX_CLI_OUTPUT {
            return refuse("plugin returned an invalid or oversized CLI result");
        }
        Ok(json!({"denied":false,"status":exit,"stdout":stdout,"stderr":stderr}))
    }

    // ---- HTTP API -------------------------------------------------------------------
    /// The size limit the declared API routes of this plugin allow for a request body.
    pub fn plugin_api_limit(&self, id: &str) -> usize {
        let state = self.registry.state.read();
        state
            .ok()
            .and_then(|s| s.entries.get(id).map(|e| e.manifest.api()))
            .map(|apis: Vec<ApiMeta>| {
                apis.iter().map(|a| a.max_body_bytes).max().unwrap_or(0) as usize
            })
            .unwrap_or(0)
    }
    pub async fn plugin_api(
        &self,
        actor: &str,
        id: &str,
        method: &str,
        subpath: &str,
        query: &str,
        body: &[u8],
    ) -> Result<ApiReply> {
        let json_reply = |status: u16, value: Value| ApiReply {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: serde_json::to_vec(&value).unwrap_or_default(),
        };
        self.permit(actor, "read", None)?;
        let (apis, phase, manifest) = {
            let state = self
                .registry
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("poisoned"))?;
            match state.entries.get(id) {
                Some(entry) => (entry.manifest.api(), entry.phase, entry.manifest.clone()),
                None => return Ok(json_reply(404, json!({"error":"plugin is not installed"}))),
            }
        };
        let path = format!("/{subpath}");
        let on_path: Vec<_> = apis
            .iter()
            .flat_map(|a| a.routes.iter().map(move |r| (r, a.max_body_bytes)))
            .filter(|(r, _)| r.path == path)
            .collect();
        let Some((route, max_body)) = on_path.iter().find(|(r, _)| r.method == method) else {
            return Ok(if on_path.is_empty() {
                json_reply(404, json!({"error":"route is not declared by this plugin"}))
            } else {
                json_reply(
                    405,
                    json!({"error":"method is not declared for this route"}),
                )
            });
        };
        // Authority first: an actor without the route's capability learns nothing about
        // whether the plugin is enabled (403 before 503).
        self.permit(actor, &route.capability, Some(&manifest.work_type()))?;
        if phase != Phase::Enabled {
            return Ok(json_reply(
                503,
                json!({"error":"plugin is installed but disabled"}),
            ));
        }
        if body.len() as u64 > *max_body {
            return Ok(json_reply(
                413,
                json!({"error":"request body exceeds the route limit"}),
            ));
        }
        let parsed = if body.is_empty() {
            Value::Null
        } else {
            match serde_json::from_slice::<Value>(body) {
                Ok(v) => v,
                Err(_) => {
                    return Ok(json_reply(
                        400,
                        json!({"error":"request body must be JSON"}),
                    ));
                }
            }
        };
        let mut params = serde_json::Map::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if params.len() >= MAX_QUERY_PARAMS {
                return Ok(json_reply(
                    400,
                    json!({"error":"too many query parameters"}),
                ));
            }
            params.insert(percent_decode(k), Value::String(percent_decode(v)));
        }
        let (plugin, _, _guard) = self.begin_call(id)?;
        self.record(actor, "plugin_api", id, &format!("{method} {path}"))?;
        let reply = plugin::invoke(
            &plugin.config,
            json!({"api_version":manifest::PROTOCOL,"operation":"api","method":method,"path":path,"query":params,"body":parsed,"principal":actor}),
        )
        .await;
        let reply = match reply {
            Ok(r) => r,
            Err(_) => return Ok(json_reply(502, json!({"error":"plugin call failed"}))),
        };
        Ok(validate_api_reply(&reply).unwrap_or_else(|| {
            json_reply(
                502,
                json!({"error":"plugin returned an invalid API response"}),
            )
        }))
    }

    // ---- MCP ------------------------------------------------------------------------
    fn mcp_entries(&self, actor: &str) -> Vec<(String, Manifest, McpMeta)> {
        let Ok(state) = self.registry.state.read() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in state.entries.values().filter(|e| e.phase == Phase::Enabled) {
            if self
                .permit(actor, "execute", Some(&entry.manifest.work_type()))
                .is_err()
            {
                continue;
            }
            for tool in entry.manifest.mcp() {
                out.push((
                    entry.manifest.mcp_name(&tool.tool_name),
                    entry.manifest.clone(),
                    tool,
                ));
            }
        }
        out
    }
    pub fn plugin_tools(&self, actor: &str) -> Result<Value> {
        self.permit(actor, "read", None)?;
        Ok(Value::Array(
            self.mcp_entries(actor)
                .into_iter()
                .map(|(name, _, t)| {
                    json!({"name":name,"description":t.description,"input_schema":t.input_schema,"read_only":t.read_only})
                })
                .collect(),
        ))
    }
    pub async fn plugin_mcp(&self, actor: &str, tool: &str, arguments: Value) -> Result<Value> {
        self.permit(actor, "execute", None)?;
        let (_, manifest, meta) = self
            .mcp_entries(actor)
            .into_iter()
            .find(|(name, _, _)| name == tool)
            .ok_or(PluginError("unknown or disabled plugin tool".into()))?;
        let validator = jsonschema::validator_for(&meta.input_schema)
            .map_err(|_| PluginError("plugin tool schema is invalid".into()))?;
        if !validator.is_valid(&arguments) {
            return Err(
                Invalid("invalid tool arguments; inspect the published input schema").into(),
            );
        }
        let (plugin, _, _guard) = self.begin_call(&manifest.id)?;
        self.record(actor, "plugin_mcp", &manifest.id, &meta.tool_name)?;
        let reply = plugin::invoke(
            &plugin.config,
            json!({"api_version":manifest::PROTOCOL,"operation":"mcp","tool":meta.tool_name,"arguments":arguments,"principal":actor}),
        )
        .await?;
        let output = reply
            .get("output")
            .cloned()
            .context("plugin tool output missing")?;
        Ok(json!({"output":output,"is_error":reply["is_error"].as_bool().unwrap_or(false)}))
    }

    // ---- Dashboard assets -----------------------------------------------------------
    /// A declared asset of an enabled plugin, read from the release and re-verified against
    /// its manifest digest on every request.
    pub async fn plugin_asset(&self, id: &str, path: &str) -> Result<(&'static str, Vec<u8>)> {
        let (resource, version) = {
            let state = self
                .registry
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("poisoned"))?;
            let entry = state
                .entries
                .get(id)
                .filter(|e| e.phase == Phase::Enabled)
                .ok_or(PluginError("asset not found".into()))?;
            let resource = entry
                .manifest
                .served_asset(path)
                .cloned()
                .ok_or(PluginError("asset not found".into()))?;
            (resource, entry.manifest.version.clone())
        };
        let kind =
            content_type(&resource.path).ok_or(PluginError("asset type not allowed".into()))?;
        let file = absolute(self.paths.plugin_release(id, &version)).join(&resource.path);
        let bytes =
            tokio::task::spawn_blocking(move || read_regular(&file, MAX_FILE_BYTES)).await??;
        if manifest::digest_text(&bytes) != resource.digest {
            return refuse("asset failed its digest check");
        }
        Ok((kind, bytes))
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Validate a protocol-2 `api` reply against the status, header and content-type
/// allow-lists. `None` means the plugin broke the contract.
fn validate_api_reply(reply: &Value) -> Option<ApiReply> {
    let status = reply["status"]
        .as_u64()
        .filter(|s| (200..=599).contains(s) && !(300..=399).contains(s))? as u16;
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in reply
        .get("headers")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let name = name.to_ascii_lowercase();
        let value = value.as_str()?;
        if !RESPONSE_HEADERS.contains(&name.as_str())
            || value.len() > 256
            || value.contains(['\r', '\n', '\0'])
        {
            return None;
        }
        headers.push((name, value.to_owned()));
    }
    let body = reply.get("body").cloned().unwrap_or(Value::Null);
    let declared = headers
        .iter()
        .find(|(n, _)| n == "content-type")
        .map(|(_, v)| v.clone());
    let (kind, bytes) = match (&body, declared) {
        (Value::String(text), Some(kind)) => (kind, text.clone().into_bytes()),
        (value, None) => (
            "application/json".to_owned(),
            serde_json::to_vec(value).ok()?,
        ),
        (value, Some(kind)) => (kind, serde_json::to_vec(value).ok()?),
    };
    let essence = kind.split(';').next()?.trim().to_ascii_lowercase();
    if !RESPONSE_TYPES.contains(&essence.as_str()) {
        return None;
    }
    headers.retain(|(n, _)| n != "content-type");
    headers.push(("content-type".into(), kind));
    Some(ApiReply {
        status,
        headers,
        body: bytes,
    })
}

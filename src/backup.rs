//! Coherent offline backups of native DBOS state and its private Forge home.
use crate::{
    safety::{self, HomeLock},
    settings::{self, Overrides, Paths, Settings},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Read,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

const INCOMPLETE: &str = settings::RESTORE_MARKER;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format_version: u32,
    home_id: String,
    postgres_major: u32,
    files: BTreeMap<String, String>,
}

/// Where the copied home keeps the paths whose content needs special handling,
/// relative to the home. Every durable runtime directory must live inside the home
/// for an archive to be complete, so anything else is refused rather than half-saved.
#[derive(Clone)]
struct Layout {
    harness: PathBuf,
    owner_lock: PathBuf,
    /// Disposable by definition, so never archived.
    cache: Option<PathBuf>,
    /// Plugin install staging: unfinished installs are debris, never archived.
    plugin_staging: Option<PathBuf>,
}
impl Layout {
    fn of(paths: &Paths) -> Result<Self> {
        let inside = |what: &str, path: &Path| {
            paths.relative(path).ok_or_else(|| {
                anyhow::Error::from(crate::settings::ConfigError(format!(
                    "Backup requires paths.{what} inside the Forge home ({} is outside it); relocate it under the home or back it up separately",
                    path.display()
                )))
            })
        };
        for (what, path) in [
            ("state", &paths.state),
            ("outputs", &paths.outputs),
            ("effect_locks", &paths.effect_locks),
            ("dbos_identity", &paths.dbos_identity),
            ("dbos_admission", &paths.dbos_admission),
            // Plugin releases are part of the home's durable state (the registry rows
            // live in the admission store): an outside location is refused, never skipped.
            ("plugins", &paths.plugins),
        ] {
            inside(what, path)?;
        }
        Ok(Self {
            harness: inside("harness", &paths.harness)?,
            owner_lock: inside("state", &paths.owner_lock())?,
            cache: paths.relative(&paths.cache),
            plugin_staging: paths.relative(&paths.plugin_staging()),
        })
    }
}

fn private_dir(path: &Path) -> Result<()> {
    let directory = safety::directory(path)?;
    let meta = directory.metadata()?;
    ensure!(
        meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "Backup directories must be private and owned by the effective user"
    );
    Ok(())
}

fn new_dir(path: &Path) -> Result<()> {
    let parent = path.parent().context("Directory requires a parent")?;
    safety::directory(parent)?;
    std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700).create(path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.nlink() == 1
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0,
        "Backup refuses links, special files or nonprivate files"
    );
    Ok(file)
}

/// Harness workspaces (`state/harness/<id>/workspace/**`) contain files created by an
/// external program, so modes, links and special files are not under Forge's control.
/// Rule: those subtrees are archived as normalized private regular files and
/// directories; symlinks, special files and unreadable entries are skipped, never
/// followed and never fatal. Everything else keeps the strict private-tree checks.
fn in_workspace(layout: &Layout, relative: &Path) -> bool {
    relative.strip_prefix(&layout.harness).is_ok_and(|rest| {
        let parts: Vec<_> = rest.components().collect();
        parts.len() >= 2 && parts[1].as_os_str() == "workspace"
    })
}

// Codex owns --output-last-message and older invocations created this file
// with native umask permissions. Normalize only this known native output path.
fn native_harness_result(layout: &Layout, relative: &Path) -> bool {
    relative.strip_prefix(&layout.harness).is_ok_and(|rest| {
        let parts: Vec<_> = rest.components().collect();
        parts.len() == 2 && parts[1].as_os_str() == "result.txt"
    })
}

#[cfg(test)]
type TestHook = Box<dyn FnOnce() + Send>;
#[cfg(test)]
static COPY_TREE_AFTER_METADATA_HOOK: std::sync::OnceLock<std::sync::Mutex<Option<TestHook>>> =
    std::sync::OnceLock::new();
#[cfg(test)]
fn set_copy_tree_after_metadata_hook(hook: Option<Box<dyn FnOnce() + Send>>) {
    *COPY_TREE_AFTER_METADATA_HOOK
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap() = hook;
}
#[cfg(test)]
fn after_copy_tree_metadata() {
    if let Some(hook) = COPY_TREE_AFTER_METADATA_HOOK
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap()
        .take()
    {
        hook();
    }
}
#[cfg(not(test))]
fn after_copy_tree_metadata() {}

fn private_directory_fd(directory: &File) -> Result<()> {
    let meta = directory.metadata()?;
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "Backup directories must be private and owned by the effective user"
    );
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path, root: &Path, layout: &Layout) -> Result<()> {
    let directory = safety::directory(source)?;
    copy_tree_fd(&directory, destination, source.strip_prefix(root)?, layout)
}

/// Copy through retained source directory descriptors. Every source child is
/// classified and rebound by name around its open/read so a replacement cannot
/// redirect recursion through a workspace-controlled symlink.
fn copy_tree_fd(source: &File, destination: &Path, relative: &Path, layout: &Layout) -> Result<()> {
    let lenient = in_workspace(layout, relative);
    if !lenient {
        private_directory_fd(source)?;
    }
    for entry in fs::read_dir(format!("/proc/self/fd/{}", source.as_raw_fd()))? {
        let name = entry?.file_name();
        let child_relative = relative.join(&name);
        if child_relative == layout.owner_lock
            || layout.cache.as_deref() == Some(&child_relative)
            || layout.plugin_staging.as_deref() == Some(&child_relative)
        {
            continue;
        }
        let target = destination.join(&name);
        let is_directory = safety::entry_is_directory(source, &name)?;
        after_copy_tree_metadata();
        if is_directory {
            let opened = safety::open_at(
                source,
                &name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            );
            let child = match opened {
                Ok(child) => child,
                Err(_error) if lenient => continue,
                Err(error) => return Err(error.into()),
            };
            safety::rebind_entry(source, &child, &name)?;
            if !in_workspace(layout, &child_relative) {
                private_directory_fd(&child)?;
            }
            new_dir(&target)?;
            copy_tree_fd(&child, &target, &child_relative, layout)?;
        } else {
            let normalized = lenient || native_harness_result(layout, &child_relative);
            let opened = safety::open_at(
                source,
                &name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            );
            let Ok(mut input) = opened else {
                if normalized {
                    continue;
                }
                return Err(anyhow::anyhow!("Backup refuses raced source entry"));
            };
            safety::rebind_entry(source, &input, &name)?;
            let meta = input.metadata()?;
            if !meta.is_file() {
                if normalized {
                    continue;
                }
                anyhow::bail!("Backup refuses links, special files or nonprivate files");
            }
            if !normalized
                && (meta.nlink() != 1
                    || meta.uid() != unsafe { libc::geteuid() }
                    || meta.mode() & 0o077 != 0)
            {
                anyhow::bail!("Backup refuses links, special files or nonprivate files");
            }
            let mode = if meta.mode() & 0o100 != 0 {
                0o700
            } else {
                0o600
            };
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&target)?;
            std::io::copy(&mut input, &mut output)?;
            output.sync_all()?;
            safety::rebind_entry(source, &input, &name)?;
        }
    }
    File::open(destination)?.sync_all()?;
    Ok(())
}

fn inventory(path: &Path, root: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
    private_dir(path)?;
    for entry in fs::read_dir(path)? {
        let child = entry?.path();
        let relative = child.strip_prefix(root)?;
        if relative == Path::new("manifest.json") || relative == Path::new("INCOMPLETE") {
            continue;
        }
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            inventory(&child, root, files)?;
        } else {
            let mut input = regular(&child)?;
            let mut hash = Sha256::new();
            let mut bytes = [0u8; 65536];
            loop {
                let size = input.read(&mut bytes)?;
                if size == 0 {
                    break;
                }
                hash.update(&bytes[..size]);
            }
            files.insert(
                relative
                    .to_str()
                    .context("Backup paths must be UTF-8")?
                    .to_owned(),
                format!("{:x}", hash.finalize()),
            );
        }
    }
    Ok(())
}

fn validate_libpq_url(database_url: &str) -> Result<()> {
    let url = reqwest::Url::parse(database_url)
        .map_err(|_| anyhow::anyhow!("Invalid explicit PostgreSQL URL"))?;
    ensure!(
        matches!(url.scheme(), "postgres" | "postgresql")
            && !url.path().trim_matches('/').is_empty()
            && url.fragment().is_none(),
        "An explicit PostgreSQL database URL is required"
    );
    // Keep libpq and the native runtime's connection targets identical. Refuse
    // native-driver-only aliases rather than silently selecting another target.
    let allowed = [
        "host",
        "port",
        "dbname",
        "user",
        "password",
        "application_name",
        "sslmode",
        "sslrootcert",
        "sslcert",
        "sslkey",
        "options",
    ];
    let mut seen = std::collections::BTreeSet::new();
    for (key, _) in url.query_pairs() {
        ensure!(
            allowed.contains(&key.as_ref()) && seen.insert(key.into_owned()),
            "Backup requires standard unique libpq URL options"
        );
    }
    ensure!(
        url.host_str().is_some()
            || url
                .query_pairs()
                .any(|(key, value)| key == "host" && !value.is_empty()),
        "Backup requires an explicit PostgreSQL host"
    );
    Ok(())
}

fn decode_url_part(value: &str) -> Result<String> {
    let mut output = Vec::new();
    let mut bytes = value.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|v| (v as char).to_digit(16));
            let low = bytes.next().and_then(|v| (v as char).to_digit(16));
            output.push(match (high, low) {
                (Some(high), Some(low)) => ((high << 4) | low) as u8,
                _ => return Err(anyhow::anyhow!("Invalid PostgreSQL URL escape")),
            });
        } else {
            output.push(byte);
        }
    }
    ensure!(!output.contains(&0), "PostgreSQL URL contains a null byte");
    String::from_utf8(output).context("PostgreSQL URL must encode UTF-8")
}

fn connection_env(database_url: &str) -> Result<BTreeMap<String, String>> {
    validate_libpq_url(database_url)?;
    let url = reqwest::Url::parse(database_url).expect("validated URL");
    let mut settings = BTreeMap::new();
    if let Some(host) = url.host_str() {
        settings.insert(
            "PGHOST".into(),
            decode_url_part(host.trim_start_matches('[').trim_end_matches(']'))?,
        );
    }
    if let Some(port) = url.port() {
        settings.insert("PGPORT".into(), port.to_string());
    }
    settings.insert(
        "PGDATABASE".into(),
        decode_url_part(url.path().trim_start_matches('/'))?,
    );
    if !url.username().is_empty() {
        settings.insert("PGUSER".into(), decode_url_part(url.username())?);
    }
    if let Some(password) = url.password() {
        settings.insert("PGPASSWORD".into(), decode_url_part(password)?);
    }
    for (key, value) in url.query_pairs() {
        let target = match key.as_ref() {
            "host" => "PGHOST",
            "port" => "PGPORT",
            "dbname" => "PGDATABASE",
            "user" => "PGUSER",
            "password" => "PGPASSWORD",
            "application_name" => "PGAPPNAME",
            "sslmode" => "PGSSLMODE",
            "sslrootcert" => "PGSSLROOTCERT",
            "sslcert" => "PGSSLCERT",
            "sslkey" => "PGSSLKEY",
            "options" => "PGOPTIONS",
            _ => unreachable!("validated option"),
        };
        ensure!(!value.contains('\0'), "PostgreSQL URL contains a null byte");
        settings.insert(target.into(), value.into_owned());
    }
    for key in ["PGHOST", "PGPORT", "PGDATABASE", "PGUSER"] {
        ensure!(
            settings.get(key).is_some_and(|value| !value.is_empty()),
            "Backup requires explicit PostgreSQL host, port, database and user"
        );
    }
    ensure!(
        settings["PGPORT"]
            .parse::<u16>()
            .ok()
            .is_some_and(|port| port > 0),
        "Invalid explicit PostgreSQL port"
    );
    Ok(settings)
}

async fn pg(program: &str, args: &[String], database_url: &str) -> Result<Vec<u8>> {
    let settings = connection_env(database_url)?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("PG") {
            command.env_remove(name);
        }
    }
    command
        .envs(settings)
        .env("PGCONNECT_TIMEOUT", "10")
        .env("PGPASSFILE", "/dev/null")
        .env("PGSERVICEFILE", "/dev/null")
        .env("PGAPPNAME", "agent-forge-offline-backup");
    let output = tokio::time::timeout(Duration::from_secs(300), command.output()).await
        .map_err(|_| anyhow::anyhow!("PostgreSQL backup utility timed out; incomplete state was retained"))?
        .map_err(|_| anyhow::anyhow!("PostgreSQL utility unavailable; install matching-major psql, pg_dump and pg_restore on PATH"))?;
    ensure!(
        output.status.success(),
        "PostgreSQL utility failed; verify matching-major tools and explicit database access (private diagnostics suppressed)"
    );
    ensure!(
        output.stdout.len() < 1024 * 1024,
        "PostgreSQL utility returned excessive metadata"
    );
    Ok(output.stdout)
}

async fn query(database_url: &str, sql: &str) -> Result<String> {
    let output = pg(
        "psql",
        &[
            "-X".into(),
            "--no-password".into(),
            "-A".into(),
            "-t".into(),
            "-v".into(),
            "ON_ERROR_STOP=1".into(),
            "-c".into(),
            sql.into(),
        ],
        database_url,
    )
    .await?;
    String::from_utf8(output)
        .map(|value| value.trim().to_owned())
        .context("Invalid PostgreSQL metadata encoding")
}

async fn major(database_url: &str) -> Result<u32> {
    let server = query(database_url, "SHOW server_version_num")
        .await?
        .parse::<u32>()?
        / 10000;
    for tool in ["psql", "pg_dump", "pg_restore"] {
        let version = String::from_utf8(pg(tool, &["--version".into()], database_url).await?)?;
        let client = version
            .split_whitespace()
            .find(|part| part.as_bytes().first().is_some_and(u8::is_ascii_digit))
            .and_then(|version| version.split('.').next())
            .and_then(|value| value.parse::<u32>().ok());
        ensure!(
            client == Some(server),
            "PostgreSQL client and server major versions must match"
        );
    }
    Ok(server)
}

fn sqlite_check(paths: &Paths, home_id: &str) -> Result<()> {
    drop(crate::dbos::admission::AdmissionStore::read_only(
        paths, home_id,
    )?);
    let path = paths.dbos_admission.clone();
    regular(&path)?;
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let status: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    ensure!(
        status == "ok",
        "Forge admission database failed integrity validation"
    );
    connection.close().map_err(|(_, error)| error)?;
    Ok(())
}

/// Longest an online backup holds admissions paused for the database dump.
const DUMP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Back up while the owning executor is stopped. Partial destinations are
/// retained with INCOMPLETE; they are never accepted for restoration.
pub async fn backup(settings: Settings, destination: PathBuf) -> Result<()> {
    for note in outside_notes(&settings.paths) {
        eprintln!("note: {note}");
    }
    backup_with(settings, destination, None).await
}

/// Locations a backup deliberately does not archive because they lie outside the home.
/// `cache` is disposable by definition; `logs` are diagnostics, not recovery state; both
/// are named loudly rather than skipped silently. (Durable directories outside the home
/// are refused outright by the layout check.)
pub fn outside_notes(paths: &Paths) -> Vec<String> {
    let mut notes = Vec::new();
    if paths.relative(&paths.logs).is_none() {
        notes.push(format!(
            "paths.logs ({}) is outside the home and is NOT in this backup; archive it separately if you need those diagnostics",
            paths.logs.display()
        ));
    }
    if paths.relative(&paths.cache).is_none() {
        notes.push(format!(
            "paths.cache ({}) is outside the home and is not archived (the cache is disposable)",
            paths.cache.display()
        ));
    }
    notes
}

/// A backup taken while the service that owns the home keeps running. It cannot hold
/// the exclusive home lock (the running service does), so it orders the two copies
/// for crash consistency instead: the PostgreSQL dump first, then the home while the
/// admission store's single connection is held, so no admission, receipt or audit write
/// can interleave with copying its file. The result equals a crash at the dump instant:
/// work in flight restores as recoverable, and effects that may have happened stay
/// uncertain (RECOVERY_REQUIRED) rather than being replayed.
pub(crate) async fn backup_online(
    settings: Settings,
    destination: PathBuf,
    core: &crate::core::Core,
) -> Result<()> {
    // Lifecycle operations can hold the plugin lock while committing admission
    // writes. Use the same order and hold it from release verification to copy.
    let lifecycle = core.registry.lock_lifecycle().await;
    backup_with(settings, destination, Some((&core.store, lifecycle))).await
}

async fn backup_with(
    settings: Settings,
    destination: PathBuf,
    online: Option<(
        &std::sync::Arc<crate::dbos::admission::AdmissionStore>,
        tokio::sync::OwnedMutexGuard<()>,
    )>,
) -> Result<()> {
    let paths = &settings.paths;
    let home = paths.home.clone();
    let database_url = settings.database.url()?.to_owned();
    let layout = Layout::of(paths)?;
    ensure!(
        home.is_absolute() && destination.is_absolute(),
        "Backup requires absolute home and destination paths"
    );
    private_dir(&home)?;
    ensure!(
        !home.join(INCOMPLETE).exists(),
        "Home contains an incomplete restore"
    );
    let _lock = if online.is_none() {
        Some(HomeLock::acquire(paths)?)
    } else {
        None
    };
    let policy = crate::core::load_policy(&home)?;
    let runtime = settings.runtime_config(&policy)?;
    let identity: Value = serde_json::from_slice(&safety::read_private(&paths.dbos_identity)?)?;
    ensure!(
        identity["database_binding"].as_str() == Some(runtime.database_binding().as_str())
            && identity["home_id"].as_str() == Some(runtime.home_id.as_str())
            && identity["application"].as_str() == Some(runtime.application.as_str())
            && identity["schema"].as_str() == Some(runtime.schema.as_str())
            && identity["executor_id"].as_str() == Some(runtime.executor_id.as_str())
            && identity["app_version"].as_str() == Some(runtime.app_version.as_str())
            && identity["home_origin"].as_str() == home.canonicalize()?.to_str(),
        "Backup identity does not match the stopped home and explicit database"
    );
    sqlite_check(paths, &runtime.home_id)?;
    // Never archive a plugin release that no longer matches its manifest digests.
    crate::plugin_registry::verify_releases(paths, false)?;
    let parent = destination
        .parent()
        .context("Backup requires a destination parent")?;
    safety::directory(parent)?;
    ensure!(
        !parent.canonicalize()?.starts_with(home.canonicalize()?),
        "Backup destination must be outside the source home"
    );
    let server_major = major(&database_url).await?;
    new_dir(&destination)?;
    safety::write_new(
        &destination.join("INCOMPLETE"),
        b"Backup has not completed\n",
    )?;
    new_dir(&destination.join("home"))?;
    if online.is_none() {
        copy_tree(&home, &destination.join("home"), &home, &layout)?;
    }
    let dump = destination.join("database.dump");
    // pg_dump opens the private precreated file, keeping its mode 0600.
    safety::write_new(&dump, b"")?;
    let dump_args: Vec<String> = vec![
        "--no-password".into(),
        "--no-owner".into(),
        "--no-privileges".into(),
        "--format=custom".into(),
        format!("--schema={}", runtime.schema),
        format!(
            "--file={}",
            dump.to_str().context("Backup path must be UTF-8")?
        ),
    ];
    if let Some((store, lifecycle)) = online {
        // One consistent cut: the admission store's single writer is held across BOTH
        // the PostgreSQL dump and the copy of the home. No admission, submission mark,
        // receipt or audit write can commit in between, so every submission the copied
        // store records as confirmed was committed to PostgreSQL before the dump began
        // (a submission acknowledged afterwards is simply recorded as not yet confirmed,
        // and the bridge confirms it idempotently after a restore). New admissions pause
        // for the duration of the dump (bounded by DUMP_TIMEOUT).
        let (from, to, root, layout) = (
            home.clone(),
            destination.join("home"),
            home.clone(),
            layout.clone(),
        );
        let handle = tokio::runtime::Handle::current();
        let url = database_url.clone();
        let store = store.clone();
        tokio::task::spawn_blocking(move || {
            // Keep the guard in the cut even if its async caller is dropped.
            let _lifecycle = lifecycle;
            let mut result: Result<()> = Ok(());
            store
                .write(|_connection| {
                    result = handle
                        .block_on(async {
                            tokio::time::timeout(DUMP_TIMEOUT, pg("pg_dump", &dump_args, &url))
                                .await
                                .map_err(|_| {
                                    anyhow::anyhow!(
                                        "pg_dump exceeded {}s while admissions were paused; backup aborted",
                                        DUMP_TIMEOUT.as_secs()
                                    )
                                })?
                        })
                        .and_then(|_| regular(&dump).and_then(|f| Ok(f.sync_all()?)))
                        .and_then(|()| copy_tree(&from, &to, &root, &layout));
                    Ok(())
                })
                .map_err(|e| anyhow::anyhow!("admission store unavailable: {e}"))?;
            result
        })
        .await??;
    } else {
        pg("pg_dump", &dump_args, &database_url).await?;
        regular(&dump)?.sync_all()?;
    }
    let mut files = BTreeMap::new();
    inventory(&destination, &destination, &mut files)?;
    let manifest = Manifest {
        format_version: 1,
        home_id: runtime.home_id,
        postgres_major: server_major,
        files,
    };
    safety::write_new(
        &destination.join("manifest.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::remove_file(destination.join("INCOMPLETE"))?;
    File::open(&destination)?.sync_all()?;
    Ok(())
}

/// Restore into a new home and empty PostgreSQL database. Source archive is
/// immutable. An interrupted restore retains a marker that refuses startup.
/// The restored home is laid out from the archived `config.json` (relative paths
/// resolve against the new home) with the current flags and environment applied.
pub async fn restore(
    home: PathBuf,
    source: PathBuf,
    overrides: &Overrides,
    env: settings::Env<'_>,
) -> Result<()> {
    ensure!(
        home.is_absolute() && source.is_absolute(),
        "Restore requires absolute home and source paths"
    );
    private_dir(&source)?;
    let parent = home
        .parent()
        .context("Restore requires a destination parent")?;
    safety::directory(parent)?;
    ensure!(
        !parent.canonicalize()?.starts_with(source.canonicalize()?),
        "Restore destination must be outside its backup archive"
    );
    ensure!(
        !source.join("INCOMPLETE").exists(),
        "Cannot restore an incomplete backup"
    );
    let manifest: Manifest =
        serde_json::from_slice(&safety::read_private(&source.join("manifest.json"))?)?;
    ensure!(manifest.format_version == 1, "Unsupported backup format");
    let mut files = BTreeMap::new();
    inventory(&source, &source, &mut files)?;
    ensure!(files == manifest.files, "Backup digest inventory mismatch");
    let source_home = source.join("home");
    let policy = crate::core::load_policy(&source_home)?;
    let settings = Settings::resolve(home.clone(), &policy, overrides, env)?;
    let paths = &settings.paths;
    let layout = Layout::of(paths)?;
    let database_url = settings.database.url()?.to_owned();
    let archived = |path: &Path| -> Result<String> {
        Ok(format!(
            "home/{}",
            paths
                .relative(path)
                .and_then(|p| p.to_str().map(str::to_owned))
                .context("Backup paths must be UTF-8 and inside the home")?
        ))
    };
    ensure!(
        files.contains_key("database.dump")
            && files.contains_key("home/config.json")
            && files.contains_key(&archived(&paths.dbos_admission)?)
            && files.contains_key(&archived(&paths.dbos_identity)?),
        "Backup is missing required state"
    );
    let runtime = settings.runtime_config(&policy)?;
    ensure!(
        runtime.home_id == manifest.home_id,
        "Backup identity mismatch"
    );
    let source_paths = Paths::resolve(&source_home, &policy.paths, env)?;
    sqlite_check(&source_paths, &runtime.home_id)?;
    let server_major = major(&database_url).await?;
    ensure!(
        server_major == manifest.postgres_major,
        "Restore requires the backup's PostgreSQL major version"
    );
    let empty = query(&database_url,"SELECT count(*) FROM pg_namespace WHERE nspname NOT IN ('public','information_schema') AND nspname !~ '^pg_'").await?;
    let public_empty = query(&database_url,"SELECT (SELECT count(*) FROM pg_class c JOIN pg_namespace n ON c.relnamespace=n.oid WHERE n.nspname='public') + (SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON p.pronamespace=n.oid WHERE n.nspname='public') + (SELECT count(*) FROM pg_type t JOIN pg_namespace n ON t.typnamespace=n.oid WHERE n.nspname='public')").await?;
    ensure!(
        empty == "0" && public_empty == "0",
        "Restore requires an empty target PostgreSQL database"
    );
    // This fails on an existing destination, even an empty directory. Never
    // merge a snapshot into another identity or overwrite its lock.
    new_dir(&home)?;
    safety::write_new(
        &home.join(INCOMPLETE),
        b"Restore incomplete; do not start this home\n",
    )?;
    copy_tree(&source_home, &home, &source_home, &layout)?;
    let _lock = HomeLock::acquire(paths)?;
    let mut copied = BTreeMap::new();
    inventory(&home, &home, &mut copied)?;
    copied.remove(INCOMPLETE);
    copied.remove(
        layout
            .owner_lock
            .to_str()
            .context("Backup paths must be UTF-8")?,
    );
    let expected: BTreeMap<String, String> = manifest
        .files
        .iter()
        .filter_map(|(name, digest)| {
            name.strip_prefix("home/")
                .map(|name| (name.to_owned(), digest.clone()))
        })
        .collect();
    ensure!(
        copied == expected,
        "Archive changed while copying the restored home"
    );
    sqlite_check(paths, &runtime.home_id)?;
    // Plugin releases are verified against their own manifest digests and re-frozen
    // read-only (a copy keeps contents and the execute bit, not read-only modes).
    crate::plugin_registry::verify_releases(paths, true)?;
    // Restore only a private staged dump whose bytes were checked after copy.
    // An archive edited during network preflight cannot change restored state.
    let dump = paths.restore_dump();
    let mut dump_source = regular(&source.join("database.dump"))?;
    let mut dump_target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&dump)?;
    std::io::copy(&mut dump_source, &mut dump_target)?;
    dump_target.sync_all()?;
    File::open(&paths.state)?.sync_all()?;
    let mut staged_dump = regular(&dump)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let size = staged_dump.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    ensure!(
        manifest.files["database.dump"] == format!("{:x}", digest.finalize()),
        "Archive dump changed while copying"
    );
    pg(
        "pg_restore",
        &[
            "--no-password".into(),
            "--no-owner".into(),
            "--no-privileges".into(),
            "--single-transaction".into(),
            // --dbname selects direct restoration; empty value uses PGDATABASE.
            "--dbname=".into(),
            dump.to_str().context("Backup path must be UTF-8")?.into(),
        ],
        &database_url,
    )
    .await?;
    let schema_exists = query(
        &database_url,
        &format!(
            "SELECT count(*) FROM pg_namespace WHERE nspname='{}'",
            runtime.schema
        ),
    )
    .await?;
    ensure!(
        schema_exists == "1",
        "Restored database is missing its exact Forge schema"
    );
    sqlite_check(paths, &runtime.home_id)?;
    let destination_identity = json!({"format_version":1,"home_id":runtime.home_id,
        "home_origin":home.canonicalize()?.to_str().context("Restore home must be UTF-8")?,
        "database_binding":runtime.database_binding(),"application":runtime.application,
        "schema":runtime.schema,"app_version":runtime.app_version,"executor_id":runtime.executor_id});
    let staged_identity = paths.identity_staging();
    safety::write_new(
        &staged_identity,
        &crate::canonical::to_vec(&destination_identity)?,
    )?;
    fs::rename(staged_identity, &paths.dbos_identity)?;
    File::open(&paths.state)?.sync_all()?;
    fs::remove_file(dump)?;
    File::open(&paths.state)?.sync_all()?;
    // Consistency report (never fatal, never a replay): admitted runs whose native
    // workflow is absent become RECOVERY_REQUIRED with a reconcile action at first start.
    match crate::lost_runs::restored_report(paths, &runtime).await {
        Ok(lost) if !lost.is_empty() => eprintln!(
            "note: {} admitted run(s) have no native workflow in the restored database; they will be RECOVERY_REQUIRED (reconcile with evidence, never replayed): {}",
            lost.len(),
            lost.join(", ")
        ),
        Ok(_) => {}
        Err(_) => eprintln!(
            "note: restored admission/workflow consistency could not be checked; the service checks again at startup"
        ),
    }
    fs::remove_file(home.join(INCOMPLETE))?;
    File::open(&home)?.sync_all()?;
    if let Some(parent) = home.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// Test hook: the home-copy part of `backup` (layout check, plugin verification, tree
/// copy) without PostgreSQL, so plugin archiving is testable without a database.
#[cfg(test)]
pub(crate) fn archive_home_for_test(paths: &Paths, destination: &Path) -> Result<()> {
    let layout = Layout::of(paths)?;
    crate::plugin_registry::verify_releases(paths, false)?;
    new_dir(destination)?;
    copy_tree(&paths.home, destination, &paths.home, &layout)
}
/// Test hook: the verification a restore applies to the copied home.
#[cfg(test)]
pub(crate) fn verify_restored_plugins_for_test(paths: &Paths) -> Result<usize> {
    crate::plugin_registry::verify_releases(paths, true)
}
/// Test hook: whether a layout is accepted for backup.
#[cfg(test)]
pub(crate) fn layout_check_for_test(paths: &Paths) -> Result<()> {
    Layout::of(paths).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn test_layout(root: &Path) -> Layout {
        Layout::of(&Paths::defaults(root)).unwrap()
    }
    #[test]
    fn snapshot_refuses_links_and_detects_tampering() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        new_dir(&root).unwrap();
        safety::write_new(&root.join("receipt"), b"verified").unwrap();
        let mut before = BTreeMap::new();
        inventory(&root, &root, &mut before).unwrap();
        OpenOptions::new()
            .write(true)
            .open(root.join("receipt"))
            .unwrap()
            .write_all(b"changed!")
            .unwrap();
        let mut after = BTreeMap::new();
        inventory(&root, &root, &mut after).unwrap();
        assert_ne!(before, after);
        std::os::unix::fs::symlink(root.join("receipt"), root.join("alias")).unwrap();
        assert!(inventory(&root, &root, &mut BTreeMap::new()).is_err());
    }
    #[test]
    fn harness_workspaces_are_archived_normalized_and_never_abort_backup() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("home");
        new_dir(&root).unwrap();
        for dir in [
            "state",
            "state/harness",
            "state/harness/inv",
            "state/harness/inv/workspace",
        ] {
            new_dir(&root.join(dir)).unwrap();
        }
        safety::write_new(&root.join("state/receipt"), b"strict").unwrap();
        let native_result = root.join("state/harness/inv/result.txt");
        fs::write(&native_result, b"native final answer").unwrap();
        fs::set_permissions(&native_result, fs::Permissions::from_mode(0o644)).unwrap();
        let ws = root.join("state/harness/inv/workspace");
        // What a harness can leave behind: loose directory modes, shared file modes,
        // a symlink (also to a Forge file), a FIFO and an unreadable file.
        fs::create_dir(ws.join("sub")).unwrap();
        fs::set_permissions(ws.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(ws.join("sub/note.txt"), b"artifact").unwrap();
        fs::set_permissions(ws.join("sub/note.txt"), fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(root.join("state/receipt"), ws.join("link")).unwrap();
        unsafe {
            let fifo = std::ffi::CString::new(ws.join("fifo").to_str().unwrap()).unwrap();
            libc::mkfifo(fifo.as_ptr(), 0o600);
        }
        fs::write(ws.join("secret"), b"x").unwrap();
        fs::set_permissions(ws.join("secret"), fs::Permissions::from_mode(0o000)).unwrap();
        let archive = temp.path().join("archive");
        new_dir(&archive).unwrap();
        copy_tree(&root, &archive, &root, &test_layout(&root)).unwrap();
        let copied = archive.join("state/harness/inv/workspace");
        let copied_result = archive.join("state/harness/inv/result.txt");
        assert_eq!(fs::read(&copied_result).unwrap(), b"native final answer");
        assert_eq!(fs::metadata(copied_result).unwrap().mode() & 0o077, 0);
        assert_eq!(fs::read(copied.join("sub/note.txt")).unwrap(), b"artifact");
        assert!(fs::symlink_metadata(copied.join("link")).is_err());
        assert!(fs::symlink_metadata(copied.join("fifo")).is_err());
        assert_eq!(fs::metadata(copied.join("sub")).unwrap().mode() & 0o077, 0);
        assert_eq!(
            fs::metadata(copied.join("sub/note.txt")).unwrap().mode() & 0o077,
            0
        );
        let mut files = BTreeMap::new();
        inventory(&archive, &archive, &mut files).unwrap();
        // Outside a workspace the strict rule still refuses a loose file.
        fs::write(root.join("state/loose"), b"x").unwrap();
        fs::set_permissions(root.join("state/loose"), fs::Permissions::from_mode(0o644)).unwrap();
        let other = temp.path().join("archive2");
        new_dir(&other).unwrap();
        assert!(copy_tree(&root, &other, &root, &test_layout(&root)).is_err());
    }
    #[test]
    fn copy_tree_refuses_directory_replaced_with_symlink_after_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("home");
        new_dir(&root).unwrap();
        let raced = root.join("workspace");
        fs::create_dir(&raced).unwrap();
        fs::write(raced.join("inside"), b"trusted").unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), b"must-not-copy").unwrap();
        let destination = temp.path().join("archive");
        new_dir(&destination).unwrap();
        set_copy_tree_after_metadata_hook(Some(Box::new({
            let raced = raced.clone();
            let outside = outside.clone();
            move || {
                fs::remove_dir_all(&raced).unwrap();
                std::os::unix::fs::symlink(outside, raced).unwrap();
            }
        })));
        let result = copy_tree(&root, &destination, &root, &test_layout(&root));
        set_copy_tree_after_metadata_hook(None);
        assert!(result.is_err());
        assert!(!destination.join("workspace/sentinel").exists());
    }
    #[test]
    fn connection_fields_decode_without_putting_credentials_in_arguments() {
        let fields = connection_env(&format!(
            "{}://{}:{}@[::1]:5432/forge%2Bdb?sslmode=disable",
            "postgresql", "u%2Bser", "s%3Ae%25cret"
        ))
        .unwrap();
        assert_eq!(fields["PGHOST"], "::1");
        assert_eq!(fields["PGUSER"], "u+ser");
        assert_eq!(fields["PGPASSWORD"], "s:e%cret");
        assert_eq!(fields["PGDATABASE"], "forge+db");
        let override_fields = connection_env("postgresql://base@localhost:5432/base?host=%2Ftmp%2Fpg&user=other&port=6543&dbname=restored").unwrap();
        assert_eq!(override_fields["PGHOST"], "/tmp/pg");
        assert_eq!(override_fields["PGDATABASE"], "restored");
        assert_eq!(override_fields["PGPORT"], "6543");
        assert!(
            connection_env(&format!(
                "{}://{}:{}@localhost:5432/db",
                "postgresql", "u", "p%00"
            ))
            .is_err()
        );
    }

    #[test]
    fn native_only_or_ambient_connection_options_are_refused() {
        for url in [
            "postgresql:///db",
            "postgresql://localhost/db?service=other",
            "postgresql://localhost/db?host=x&host=y",
            "postgresql://localhost/db?ssl-mode=disable",
        ] {
            assert!(validate_libpq_url(url).is_err());
        }
        assert!(validate_libpq_url("postgresql://user@localhost:5432/db?sslmode=disable").is_ok());
    }
}

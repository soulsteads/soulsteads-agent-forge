//! `soulsteads-forge service install|uninstall|status|preview`: a systemd `--user` unit
//! generated from the resolved settings (home, listener) and run by the same binary.
//!
//! The unit never contains a database credential. The PostgreSQL URL reaches the service
//! through the environment variable Forge already resolves (`FORGE_DATABASE_URL`, or the
//! `database.url_env` name from config.json) supplied by an `EnvironmentFile=` reference the
//! operator keeps private, or from the home's private config.json. An existing unit is
//! never overwritten with different content, and uninstall removes only a unit that carries
//! this tool's marker and names this home.
use crate::core::Policy;
use crate::settings::{self, Settings};
use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MANAGER_TIMEOUT: Duration = Duration::from_secs(30);

/// A unit name: lowercase letters, digits and dashes, without the suffix.
pub fn unit_name(name: Option<&str>) -> Result<String> {
    let name = name.unwrap_or(settings::SERVICE_DEFAULT_NAME);
    let ok = (1..=48).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        return settings::refuse(
            "service name must be 1-48 lowercase letters, digits or dashes, starting with a letter",
        );
    }
    Ok(format!("{name}{}", settings::SERVICE_SUFFIX))
}

/// One argument of an `ExecStart=` line, quoted for systemd.
fn quote(arg: &str) -> Result<String> {
    if arg.chars().any(char::is_control) {
        return settings::refuse("a path or value contains a control character");
    }
    let escaped: String = arg
        .chars()
        .flat_map(|c| match c {
            '\\' => vec!['\\', '\\'],
            '"' => vec!['\\', '"'],
            '%' => vec!['%', '%'],
            '$' => vec!['$', '$'],
            c => vec![c],
        })
        .collect();
    Ok(format!("\"{escaped}\""))
}

/// The environment variable names the service needs from an `EnvironmentFile`.
fn required_variables(settings: &Settings, policy: &Policy) -> Vec<String> {
    let source = settings
        .sources
        .iter()
        .find(|(k, _)| *k == "database.url")
        .map(|(_, s)| *s)
        .unwrap_or("default");
    if let Some(name) = &policy.database.url_env {
        return vec![name.clone()];
    }
    if policy.database.url.is_some() && source == "config" {
        return Vec::new();
    }
    vec![settings::ENV_DATABASE_URL.to_owned()]
}

/// Names defined by an environment file (never its values).
fn defined_names(file: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(file)
        .map_err(|_| settings::ConfigError("the environment file cannot be read".into()))?;
    Ok(text
        .lines()
        .filter_map(|l| {
            let l = l.trim_start();
            if l.starts_with('#') {
                return None;
            }
            l.split_once('=').map(|(k, _)| k.trim().to_owned())
        })
        .collect())
}

/// The unit text for `home`, or a refusal naming what is missing. `exe` is this binary.
pub fn unit_text(
    settings: &Settings,
    policy: &Policy,
    exe: &Path,
    env_file: Option<&Path>,
) -> Result<String> {
    let needed = required_variables(settings, policy);
    if !needed.is_empty() {
        let Some(file) = env_file else {
            return settings::refuse(format!(
                "the service needs {} from its environment: pass --env-file PATH, a private (mode 0600) file that defines it. The unit refers to the file and never contains the value",
                needed.join(", ")
            ));
        };
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(file)
            .map_err(|_| settings::ConfigError("the environment file does not exist".into()))?;
        if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
            return settings::refuse(
                "the environment file must be a regular file with mode 0600 (no group or other access)",
            );
        }
        let defined = defined_names(file)?;
        let missing: Vec<&String> = needed.iter().filter(|n| !defined.contains(n)).collect();
        if !missing.is_empty() {
            return settings::refuse(format!(
                "the environment file does not define {}",
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    for p in [exe, &settings.paths.home] {
        if !p.is_absolute() {
            return settings::refuse("the executable and home must be absolute paths");
        }
    }
    let mut out = String::new();
    out.push_str(settings::SERVICE_MARKER);
    out.push('\n');
    out.push_str(&format!(
        "# Home: {}\n",
        settings.paths.home.display().to_string().replace('\n', " ")
    ));
    out.push_str("[Unit]\nDescription=Agent Forge (native DBOS work execution)\nAfter=network.target\n\n[Service]\nType=simple\n");
    if let Some(file) = env_file.filter(|_| !needed.is_empty()) {
        // An absolute reference; a missing file fails the start loudly.
        let abs = std::path::absolute(file)?;
        let text = abs.to_string_lossy();
        if text
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '%' || c == '$')
        {
            return settings::refuse(
                "the environment file path must not contain whitespace, '%' or '$'",
            );
        }
        out.push_str(&format!("EnvironmentFile={text}\n"));
    }
    out.push_str(&format!(
        "ExecStart={} --home {} serve --host {} --port {}\n",
        quote(&exe.to_string_lossy())?,
        quote(&settings.paths.home.to_string_lossy())?,
        settings.server.host,
        settings.server.port
    ));
    out.push_str("Restart=on-failure\nRestartSec=5\nKillSignal=SIGTERM\nTimeoutStopSec=90\n\n[Install]\nWantedBy=default.target\n");
    Ok(out)
}

/// The system unit deliberately does not load the Forge home as root. Forge resolves its
/// policy and listener only after systemd has dropped to `run_as`.
pub fn system_unit_text(
    home: &Path,
    exe: &Path,
    run_as: &str,
    env_file: Option<&Path>,
) -> Result<String> {
    if run_as == "root"
        || !(1..=32).contains(&run_as.len())
        || !run_as.as_bytes()[0].is_ascii_lowercase()
        || !run_as
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return settings::refuse(
            "--run-as must name a non-root account using lowercase letters, digits, '-' or '_'",
        );
    }
    for p in [exe, home] {
        if !p.is_absolute() {
            return settings::refuse("the executable and home must be absolute paths");
        }
    }
    let mut out = String::new();
    out.push_str(settings::SERVICE_MARKER);
    out.push('\n');
    out.push_str(&format!(
        "# Home: {}\n",
        home.display().to_string().replace('\n', " ")
    ));
    out.push_str("[Unit]\nDescription=Soulsteads Forge (native DBOS work execution)\nAfter=network.target\n\n[Service]\nType=simple\n");
    out.push_str(&format!("User={run_as}\n"));
    if let Some(file) = env_file {
        if !file.is_absolute() {
            return settings::refuse("the system environment file must be an absolute path");
        }
        let text = file.to_str().ok_or_else(|| {
            settings::ConfigError("the system environment file must be a UTF-8 path".into())
        })?;
        if text.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '%' | '$' | '\\' | '"' | '\'' | '*' | '?' | '[' | ']')
        }) {
            return settings::refuse(
                "the environment file path must not contain whitespace or special characters",
            );
        }
        out.push_str(&format!("EnvironmentFile={text}\n"));
    }
    out.push_str(&format!(
        "ExecStart={} --home {} serve\n",
        quote(exe.to_str().ok_or_else(|| settings::ConfigError(
            "the executable must have a UTF-8 path".into()
        ))?)?,
        quote(
            home.to_str()
                .ok_or_else(|| settings::ConfigError("the home must have a UTF-8 path".into()))?
        )?,
    ));
    out.push_str("Restart=on-failure\nRestartSec=5\nKillSignal=SIGTERM\nTimeoutStopSec=90\nNoNewPrivileges=true\n\n[Install]\nWantedBy=multi-user.target\n");
    Ok(out)
}

async fn manager(args: &[&str]) -> Result<(bool, String)> {
    let mut command = tokio::process::Command::new(settings::SYSTEMCTL);
    command
        .arg("--user")
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(MANAGER_TIMEOUT, command.output()).await {
        Ok(Ok(out)) => Ok((
            out.status.success(),
            String::from_utf8_lossy(if out.stdout.is_empty() {
                &out.stderr
            } else {
                &out.stdout
            })
            .trim()
            .chars()
            .take(300)
            .collect(),
        )),
        _ => Ok((false, "the user service manager did not answer".into())),
    }
}

async fn available() -> bool {
    manager(&["show-environment"])
        .await
        .map(|r| r.0)
        .unwrap_or(false)
}

pub struct Request<'a> {
    pub name: Option<&'a str>,
    pub env_file: Option<&'a Path>,
    /// Start (and enable) the unit after writing it.
    pub start: bool,
}

/// System-scope inputs. The caller must not resolve any Forge policy before invoking this.
pub struct SystemRequest<'a> {
    pub name: Option<&'a str>,
    pub env_file: Option<&'a Path>,
    pub run_as: &'a str,
    /// Kept explicit so accidental root-context starts can be refused.
    pub start: bool,
}

fn require_system_admin() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return settings::refuse(
            "system service installation requires root; run the command explicitly with your privilege tool",
        );
    }
    Ok(())
}

// The system manager reads an EnvironmentFile as root. Require a trusted chain,
// not just a trusted leaf that could be swapped via a writable parent directory.
fn trusted_system_path(path: &Path, directory: bool, private: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if !path.is_absolute()
        || path
            .components()
            .any(|part| part == std::path::Component::ParentDir)
    {
        return settings::refuse("system service paths must be absolute without '..'");
    }
    for (i, ancestor) in path.ancestors().enumerate() {
        let meta = std::fs::symlink_metadata(ancestor).map_err(|_| {
            settings::ConfigError(
                "a required system service file or directory does not exist".into(),
            )
        })?;
        let want_dir = i != 0 || directory;
        let masked = if i == 0 && private { 0o177 } else { 0o022 };
        if (want_dir && !meta.is_dir())
            || (!want_dir && !meta.is_file())
            || meta.uid() != 0
            || meta.mode() & masked != 0
        {
            return settings::refuse(
                "the system unit directory, executable and environment file must have root-owned, non-writable-by-others, symlink-free paths; the environment file must be private",
            );
        }
    }
    Ok(())
}

fn validate_system_account(run_as: &str) -> Result<()> {
    use std::ffi::CString;
    let name = CString::new(run_as)
        .map_err(|_| settings::ConfigError("--run-as contains an invalid account name".into()))?;
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut buffer = [0u8; 16 * 1024];
    let mut account = std::ptr::null_mut();
    // Reentrant NSS lookup; the result points to `record` only on success.
    let code = unsafe {
        libc::getpwnam_r(
            name.as_ptr(),
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut account,
        )
    };
    if code != 0 || account.is_null() {
        return settings::refuse("--run-as must name an existing non-root account");
    }
    if unsafe { (*account).pw_uid } == 0 {
        return settings::refuse("--run-as must name a non-root account");
    }
    Ok(())
}

/// Install or preview a system unit without opening or resolving Forge application state.
pub fn run_system(
    action: &str,
    home: &Path,
    exe: &Path,
    request: SystemRequest<'_>,
) -> Result<Value> {
    if request.name.is_some() {
        return settings::refuse("--name is only supported for user units");
    }
    let unit = unit_name(None)?;
    let dir = Path::new(settings::SYSTEMD_SYSTEM_DIR);
    let path = dir.join(&unit);
    let text = system_unit_text(home, exe, request.run_as, request.env_file)?;
    match action {
        "preview" => Ok(json!({"unit":unit,"path":path,"text":text})),
        "install" => {
            if request.start {
                return settings::refuse(
                    "system service install never starts the unit; use systemctl after reviewing it",
                );
            }
            require_system_admin()?;
            trusted_system_path(dir, true, false)?;
            trusted_system_path(exe, false, false)?;
            validate_system_account(request.run_as)?;
            if let Some(file) = request.env_file {
                trusted_system_path(file, false, true)?;
            }
            publish_system_unit(dir, &path, &text)?;
            Ok(
                json!({"unit":unit,"path":path,"installed":true,"started":false,"note":"The unit was written but not loaded, enabled or started. Use systemctl daemon-reload and inspect the unit before enabling it."}),
            )
        }
        _ => settings::refuse("system service action must be preview or install"),
    }
}

fn publish_system_unit(dir: &Path, path: &Path, text: &str) -> Result<()> {
    use std::io::ErrorKind;
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.is_file()
                && meta.nlink() == 1
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o022 == 0
                && meta.len() == text.len() as u64
                && std::fs::read(path).is_ok_and(|old| old == text.as_bytes())
            {
                return Ok(());
            }
            settings::refuse(
                "the unit already exists with different content or an unsafe type or ownership; it was not overwritten",
            )
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            publish_new_system_unit(dir, path, text)
        }
        Err(error) => Err(error.into()),
    }
}

// Only called in the already verified root-owned directory. A no-replace
// rename publishes the complete unit without creating a hard link.
#[cfg(target_os = "linux")]
fn publish_new_system_unit(dir: &Path, path: &Path, text: &str) -> Result<()> {
    use std::ffi::CString;
    use std::io::{ErrorKind, Write};
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    let mut random = [0u8; 16];
    getrandom::getrandom(&mut random)
        .map_err(|_| settings::ConfigError("cannot create a private unit staging name".into()))?;
    let stage = dir.join(format!(
        ".forge-install-{:032x}",
        u128::from_ne_bytes(random)
    ));
    let src = CString::new(stage.as_os_str().as_bytes())?;
    let dst = CString::new(path.as_os_str().as_bytes())?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&stage)?;
    let result = (|| -> Result<()> {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        // Both names are in the same trusted directory. On error the
        // syscall has not moved the staging file or changed the destination.
        if unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                src.as_ptr(),
                libc::AT_FDCWD,
                dst.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } != 0
        {
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::AlreadyExists {
                return settings::refuse("the unit already exists; it was not overwritten");
            }
            return Err(error.into());
        }
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(cleanup) = std::fs::remove_file(&stage) {
            return Err(error.context(format!("could not remove staged unit: {cleanup}")));
        }
        return Err(error);
    }
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn publish_new_system_unit(_dir: &Path, _path: &Path, _text: &str) -> Result<()> {
    settings::refuse("systemd service installation is supported only on Linux")
}

/// Execute one action: `preview`, `install`, `uninstall` or `status`.
pub async fn run(
    action: &str,
    settings: &Settings,
    policy: &Policy,
    request: Request<'_>,
    dir: PathBuf,
) -> Result<Value> {
    let unit = unit_name(request.name)?;
    let path: PathBuf = dir.join(&unit);
    let exe = std::env::current_exe()?;
    match action {
        "preview" => Ok(
            json!({"unit":unit,"path":path,"text":unit_text(settings, policy, &exe, request.env_file)?}),
        ),
        "status" => {
            let present = path.exists();
            let ours = present
                && std::fs::read_to_string(&path)
                    .is_ok_and(|t| t.starts_with(settings::SERVICE_MARKER));
            let ok = available().await;
            let (active, enabled) = if present && ok {
                (
                    manager(&["is-active", &unit]).await?.1,
                    manager(&["is-enabled", &unit]).await?.1,
                )
            } else {
                ("unknown".into(), "unknown".into())
            };
            Ok(
                json!({"unit":unit,"path":path,"installed":present,"managed_by_agent_forge":ours,"systemd_user_available":ok,"active":active,"enabled":enabled}),
            )
        }
        "install" => {
            let text = unit_text(settings, policy, &exe, request.env_file)?;
            if !available().await {
                return settings::refuse(
                    "systemd --user is not available here; use `service preview` to get the unit text and install it by hand",
                );
            }
            if path.exists() {
                let same = std::fs::read_to_string(&path).is_ok_and(|t| t == text);
                if !same {
                    return settings::refuse(format!(
                        "{unit} already exists with different content; it is never overwritten (choose another --name or remove it yourself)"
                    ));
                }
            } else {
                std::fs::create_dir_all(&dir)?;
                std::fs::write(&path, &text)?;
            }
            let (reloaded, why) = manager(&["daemon-reload"]).await?;
            if !reloaded {
                return settings::refuse(format!("daemon-reload failed: {why}"));
            }
            let mut started = false;
            if request.start {
                let (ok, why) = manager(&["enable", "--now", &unit]).await?;
                if !ok {
                    return settings::refuse(format!(
                        "the unit was installed but could not be started: {why}"
                    ));
                }
                started = true;
            }
            Ok(
                json!({"unit":unit,"path":path,"installed":true,"started":started,"note":"The database credential is not in the unit: it comes from the referenced environment file or the private config.json."}),
            )
        }
        "uninstall" => {
            if !path.exists() {
                return Ok(json!({"unit":unit,"path":path,"installed":false,"removed":false}));
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let home_line = format!("# Home: {}", settings.paths.home.display());
            if !text.starts_with(settings::SERVICE_MARKER) || !text.lines().any(|l| l == home_line)
            {
                return settings::refuse(format!(
                    "{unit} was not generated by soulsteads-forge for this home; it is left untouched"
                ));
            }
            if available().await {
                let _ = manager(&["disable", "--now", &unit]).await;
            }
            std::fs::remove_file(&path)?;
            if available().await {
                let _ = manager(&["daemon-reload"]).await;
            }
            Ok(json!({"unit":unit,"path":path,"installed":false,"removed":true}))
        }
        _ => settings::refuse("service action must be preview, install, uninstall or status"),
    }
}

impl crate::core::Core {
    /// The same operations through the service (API/MCP/dashboard); needs `configure`.
    pub async fn service(
        &self,
        actor: &str,
        action: &str,
        name: Option<&str>,
        env_file: Option<&str>,
        start: bool,
    ) -> Result<Value> {
        self.permit(
            actor,
            if action == "status" || action == "preview" {
                "read"
            } else {
                "configure"
            },
            None,
        )?;
        let settings = self
            .settings
            .get()
            .ok_or(crate::core::Invalid("settings unavailable"))?;
        let env_file = env_file.map(PathBuf::from);
        let result = run(
            action,
            settings,
            &self.policy,
            Request {
                name,
                env_file: env_file.as_deref(),
                start,
            },
            settings::systemd_user_dir(&settings::process_env)?,
        )
        .await?;
        if action == "install" || action == "uninstall" {
            self.record(
                actor,
                &format!("service_{action}"),
                result["unit"].as_str().unwrap_or(""),
                "",
            )?;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_environment_file_is_a_literal_path_not_a_glob() {
        for pattern in ["db*.env", "db?.env", "db[12].env"] {
            let env_file = Path::new("/etc/forge").join(pattern);
            assert!(
                system_unit_text(
                    Path::new("/srv/forge/home"),
                    Path::new("/usr/local/bin/soulsteads-forge"),
                    "forge-service",
                    Some(&env_file),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn system_preview_rejects_a_custom_unit_name() {
        let result = run_system(
            "preview",
            Path::new("/srv/forge/home"),
            Path::new("/usr/local/bin/soulsteads-forge"),
            SystemRequest {
                name: Some("alternate"),
                env_file: None,
                run_as: "forge-service",
                start: false,
            },
        );
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn system_publication_refuses_an_existing_hard_link_even_if_bytes_match() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let source = dir.join("other");
        let path = dir.join("test.service");
        std::fs::write(&source, "unit contents").unwrap();
        std::fs::hard_link(&source, &path).unwrap();
        assert!(publish_system_unit(dir, &path, "unit contents").is_err());
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "unit contents");
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 2);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn no_replace_rename_cleans_up_the_stage_on_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let path = dir.join("test.service");
        std::fs::write(&path, "existing unit").unwrap();
        let error = publish_new_system_unit(dir, &path, "new unit").unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "existing unit");
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 1);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn system_publication_never_replaces_an_existing_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let path = dir.join("test.service");
        publish_system_unit(dir, &path, "first unit").unwrap();
        publish_system_unit(dir, &path, "first unit").unwrap();
        assert!(publish_system_unit(dir, &path, "other unit").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first unit");
        let link = dir.join("foreign.service");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(publish_system_unit(dir, &link, "first unit").is_err());
        let dangling = dir.join("dangling.service");
        let absent = dir.join("absent");
        std::os::unix::fs::symlink(&absent, &dangling).unwrap();
        assert!(publish_system_unit(dir, &dangling, "first unit").is_err());
        assert!(!absent.exists());
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 3);
    }
}

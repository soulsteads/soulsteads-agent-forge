//! `agent-forge service install|uninstall|status|preview`: a systemd `--user` unit
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
                    "{unit} was not generated by agent-forge for this home; it is left untouched"
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

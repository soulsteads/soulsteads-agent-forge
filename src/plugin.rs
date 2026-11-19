use crate::containment::{self, Contained, Mode};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    os::unix::process::CommandExt,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    pub command: Vec<String>,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
    /// Names of environment variables copied from the Forge process into this plugin
    /// (for example a credential the plugin needs). Everything else is withheld; the
    /// service's own settings and credentials can never be named here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_passthrough: Vec<String>,
}
fn timeout() -> u64 {
    30
}
#[derive(Clone)]
pub struct Plugin {
    pub config: PluginConfig,
    pub description: String,
    pub schema: Value,
    /// Stdio protocol version this plugin speaks: 1 for explicitly configured
    /// executables, 2 for installed manifest plugins.
    pub api_version: u64,
}
impl Plugin {
    /// The `execute` request for one work step, in this plugin's protocol version.
    pub fn execute_request(&self, input: &Value, run_id: &str, key: &str) -> Value {
        json!({"api_version":self.api_version,"operation":"execute","input":input,"context":{"run_id":run_id,"idempotency_key":key}})
    }
}
/// Whether `name` may be copied into a plugin: a valid variable name, none of Forge's
/// own `FORGE_*` settings or credentials (the dashboard token and database URL among
/// them), and not the variable `database.url_env` points at.
pub fn passthrough_allowed(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.as_bytes()[0].is_ascii_digit()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.starts_with("FORGE_")
        && !crate::settings::withheld_env(name)
}
/// The complete, explicit environment of a plugin process: `PATH` from the settings
/// resolver, `LANG`, and only the variables the operator named in `env_passthrough`.
/// The server's environment is never inherited, so it cannot leak the database URL,
/// the dashboard token or any other credential.
pub(crate) fn plugin_environment(config: &PluginConfig) -> Vec<(String, String)> {
    let mut env = vec![
        ("PATH".to_owned(), crate::settings::harness_default_path()),
        (
            "LANG".to_owned(),
            std::env::var("LANG")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "C.UTF-8".to_owned()),
        ),
    ];
    for name in &config.env_passthrough {
        if passthrough_allowed(name)
            && !env.iter().any(|(n, _)| n == name)
            && let Ok(value) = std::env::var(name)
        {
            env.push((name.clone(), value));
        }
    }
    env
}
pub(crate) fn invocation_id(effect_key: &str) -> String {
    format!("plugin-{:x}", Sha256::digest(effect_key.as_bytes()))
}

static EPHEMERAL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn ephemeral_invocation(body: &Value) -> String {
    let digest = Sha256::digest(crate::canonical::to_vec(body).unwrap_or_default());
    let sequence = EPHEMERAL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "plugin-ephemeral-{:x}-{}-{sequence}",
        digest,
        std::process::id()
    )
}

pub(crate) fn reconcile(effect_key: &str) -> Result<()> {
    containment::reconcile_invocation(&invocation_id(effect_key))
        .context("plugin containment recovery failed")
}

/// Lifecycle calls deliberately share one deterministic identity per home and
/// operation. `Runtime::new` holds the exclusive home-owner lock while server
/// startup and shutdown call these operations serially, so a per-plugin identity
/// is neither needed nor safe after a plugin is removed from configuration.
fn lifecycle_invocation(home: &Path, operation: &str) -> Result<String> {
    if !matches!(operation, "describe" | "close") {
        bail!("unsupported plugin lifecycle operation");
    }
    use std::os::unix::ffi::OsStrExt;
    let home = home
        .canonicalize()
        .context("plugin lifecycle home could not be resolved")?;
    let mut digest = Sha256::new();
    digest.update(b"forge-plugin-lifecycle-v1\0");
    digest.update(home.as_os_str().as_bytes());
    digest.update(b"\0");
    digest.update(operation.as_bytes());
    Ok(format!("plugin-lifecycle-{:x}", digest.finalize()))
}

/// Reconcile every lifecycle identity before configuration is read. This covers
/// both a prior describe and a prior shutdown close even when its plugin entry
/// was removed before the restart.
pub fn reconcile_lifecycle(home: &Path) -> Result<()> {
    for operation in ["describe", "close"] {
        containment::reconcile_invocation(&lifecycle_invocation(home, operation)?)
            .with_context(|| format!("plugin {operation} lifecycle recovery failed"))?;
    }
    Ok(())
}

pub async fn invoke(config: &PluginConfig, body: Value) -> Result<Value> {
    let invocation = body
        .pointer("/context/idempotency_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty() && key.len() <= 256)
        .map(invocation_id)
        .unwrap_or_else(|| ephemeral_invocation(&body));
    invoke_with_invocation(config, body, invocation).await
}

async fn invoke_lifecycle(
    config: &PluginConfig,
    home: &Path,
    operation: &'static str,
) -> Result<Value> {
    invoke_with_invocation(
        config,
        json!({"api_version":1,"operation":operation}),
        lifecycle_invocation(home, operation)?,
    )
    .await
}

async fn invoke_with_invocation(
    config: &PluginConfig,
    body: Value,
    invocation: String,
) -> Result<Value> {
    if config.command.is_empty()
        || !Path::new(&config.command[0]).is_absolute()
        || !(1..=300).contains(&config.timeout_seconds)
    {
        bail!("plugin requires absolute executable and bounded timeout");
    }
    let mut contained = Contained::create(Mode::Auto, &invocation)
        .context("plugin containment unavailable; refused before dispatch")?;
    let mut command = Command::new(&config.command[0]);
    command.env_clear();
    for (name, value) in plugin_environment(config) {
        command.env(name, value);
    }
    command
        .args(&config.command[1..])
        .env("FORGE_INVOCATION_ID", &invocation)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    contained.prepare(command.as_std_mut());
    let mut child = command.spawn().context("plugin could not start")?;
    let pid = child.id().context("plugin process missing")?;
    contained.set_root(pid);
    let mut stdin = child.stdin.take().context("plugin stdin missing")?;
    let mut stdout = child
        .stdout
        .take()
        .context("plugin stdout missing")?
        .take(1024 * 1024 + 1);
    let mut stderr = child
        .stderr
        .take()
        .context("plugin stderr missing")?
        .take(65537);
    let raw = serde_json::to_vec(&body)?;
    let task = async {
        // A plugin may exit (or close stdin) without reading its request. That is a
        // plugin outcome, decided by its exit status and output below, not a transport
        // error: EPIPE/connection-reset on stdin is tolerated, anything else is a clean
        // plugin failure.
        let written = async {
            stdin.write_all(&raw).await?;
            stdin.write_all(b"\n").await?;
            stdin.shutdown().await
        }
        .await;
        let unread = match written {
            Ok(()) => false,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                true
            }
            Err(_) => bail!("plugin failed: its input could not be delivered"),
        };
        drop(stdin);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (a, b, status) = tokio::join!(
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            child.wait()
        );
        a?;
        b?;
        let status = status?;
        if unread {
            bail!("plugin closed its input before reading the request");
        }
        if out.len() > 1024 * 1024 || err.len() > 65536 || !status.success() {
            bail!("plugin failed or exceeded output limit");
        }
        let value: Value = serde_json::from_slice(&out).context("plugin returned invalid JSON")?;
        // The reply must speak the version the request asked for (1 or 2).
        let expected = body.get("api_version").and_then(Value::as_u64);
        if !matches!(expected, Some(1 | 2))
            || value.get("api_version").and_then(Value::as_u64) != expected
        {
            bail!("plugin protocol version mismatch");
        }
        Ok::<_, anyhow::Error>(value)
    };
    let result = tokio::time::timeout(Duration::from_secs(config.timeout_seconds), task).await;
    // A plugin is an effect boundary: success, timeout, malformed output, and
    // cancellation all terminate the complete contained tree before returning.
    // On a completed task `child.wait()` ran inside `task`; after timeout, bound
    // the direct-child reap as well so teardown failure cannot hang the caller.
    let root_reaped = result.is_ok();
    let termination = contained.kill(root_reaped);
    let child_reap = if root_reaped {
        Ok(())
    } else {
        match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Err(anyhow::anyhow!(
                "direct plugin child could not be reaped in time"
            )),
        }
    };
    let cleanup = contained.cleanup();
    if let Some(error) = termination.err().or(child_reap.err()).or(cleanup.err()) {
        return Err(error).context("plugin containment teardown could not be confirmed");
    }
    contained.disarm();
    result.map_err(|_| anyhow::anyhow!("plugin timeout; external outcome may be unknown"))?
}
pub async fn load(
    home: &Path,
    configs: &BTreeMap<String, PluginConfig>,
) -> Result<BTreeMap<String, Plugin>> {
    let mut plugins = BTreeMap::new();
    for (name, config) in configs {
        if !name.starts_with("plugin.")
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            bail!("invalid plugin type name");
        }
        let result = invoke_lifecycle(config, home, "describe").await?;
        let description = result["description"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 1024)
            .context("plugin description missing")?
            .to_owned();
        let schema = result["schema"].clone();
        if !schema.is_object() {
            bail!("plugin schema missing");
        }
        jsonschema::validator_for(&schema).map_err(|_| anyhow::anyhow!("invalid plugin schema"))?;
        plugins.insert(
            name.clone(),
            Plugin {
                config: config.clone(),
                description,
                schema,
                api_version: 1,
            },
        );
    }
    Ok(plugins)
}

/// Shutdown closes plugins serially under the same home-owned close identity.
/// Any unconfirmed close containment fails the shutdown instead of being ignored.
pub async fn close(home: &Path, plugins: &BTreeMap<String, Plugin>) -> Result<()> {
    for plugin in plugins.values() {
        invoke_lifecycle(&plugin.config, home, "close").await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_invocations_are_distinct_within_one_process() {
        let body = json!({"api_version":1,"operation":"describe"});
        assert_ne!(ephemeral_invocation(&body), ephemeral_invocation(&body));
    }

    /// Run a trivial plugin whose containment is made to fail in `failure`, for exactly
    /// the invocation this call derives from a unique idempotency key. Nothing else in
    /// the process can consume the injected failure.
    async fn invoke_with_injected(
        key: &str,
        failure: containment::TestFailure,
    ) -> anyhow::Result<Value> {
        let invocation = invocation_id(key);
        containment::set_test_failure(&invocation, Some(failure));
        let result = invoke(
            &PluginConfig {
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "cat >/dev/null; printf '{\"api_version\":1}'".into(),
                ],
                timeout_seconds: 1,
                env_passthrough: vec![],
            },
            json!({"api_version":1,"operation":"describe","context":{"idempotency_key":key}}),
        )
        .await;
        containment::set_test_failure(&invocation, None);
        result
    }

    #[tokio::test]
    async fn invoke_refuses_success_when_teardown_cannot_be_confirmed() {
        let result = invoke_with_injected(
            "injected-termination-proof",
            containment::TestFailure::TerminationProof,
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("containment teardown could not be confirmed")
        );
    }

    #[tokio::test]
    async fn invoke_refuses_success_when_containment_cleanup_fails() {
        let result = invoke_with_injected(
            "injected-leaf-removal",
            containment::TestFailure::LeafRemoval,
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("containment teardown could not be confirmed")
        );
    }

    #[tokio::test]
    async fn a_plugin_that_closes_stdin_unread_is_a_classified_failure() {
        // Larger than any pipe buffer, so the write can only end in a broken pipe.
        let body =
            json!({"api_version":1,"operation":"execute","context":{"pad":"x".repeat(1 << 20)}});
        let error = invoke(
            &PluginConfig {
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "exec 0<&-; printf '{\"api_version\":1}'".into(),
                ],
                timeout_seconds: 5,
                env_passthrough: vec![],
            },
            body,
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("closed its input before reading the request"),
            "{error:#}"
        );
    }

    #[test]
    fn lifecycle_recovery_reconciles_describe_and_close_without_plugin_config() {
        let home = tempfile::tempdir().unwrap();
        let describe = lifecycle_invocation(home.path(), "describe").unwrap();
        let close = lifecycle_invocation(home.path(), "close").unwrap();
        assert_ne!(describe, close);
        let mut children = Vec::new();
        for invocation in [&describe, &close] {
            children.push(
                std::process::Command::new("/bin/sh")
                    .args(["-c", "setsid sleep 60 & wait"])
                    .env("FORGE_INVOCATION_ID", invocation)
                    .spawn()
                    .unwrap(),
            );
        }
        std::thread::sleep(Duration::from_millis(50));
        reconcile_lifecycle(home.path()).unwrap();
        drop(children);
        assert!(!containment::alive(&describe, None, None).unwrap());
        assert!(!containment::alive(&close, None, None).unwrap());
    }
}

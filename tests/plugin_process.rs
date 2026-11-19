//! Real subprocess transport checks. These fixtures are not installed plugins.
use agent_forge::plugin::{PluginConfig, invoke};
use serde_json::json;
use std::time::Duration;

fn command(script: &str) -> PluginConfig {
    PluginConfig {
        command: vec!["/bin/sh".into(), "-c".into(), script.into()],
        timeout_seconds: 1,
        env_passthrough: vec![],
    }
}

#[tokio::test]
async fn executable_protocol_accepts_versioned_json_and_rejects_malformed_replies() {
    let response = invoke(
        &command("cat >/dev/null; printf '%s' '{\"api_version\":1,\"output\":{\"text\":\"native fixture\"}}'"),
        json!({"api_version":1,"operation":"execute"}),
    )
    .await
    .unwrap();
    assert_eq!(response["output"]["text"], "native fixture");
    for script in [
        "printf '%s' 'not JSON'",
        "printf '%s' '{\"api_version\":2}'",
        "printf '%s' '{}'",
        "exit 7",
    ] {
        assert!(
            invoke(
                &command(script),
                json!({"api_version":1,"operation":"describe"})
            )
            .await
            .is_err()
        );
    }
}

#[tokio::test]
async fn executable_protocol_refuses_excessive_stdout_and_stderr() {
    for script in [
        "head -c 1048577 /dev/zero",
        "head -c 65537 /dev/zero >&2; printf '%s' '{\"api_version\":1}'",
    ] {
        let outcome = tokio::time::timeout(
            Duration::from_secs(4),
            invoke(
                &command(script),
                json!({"api_version":1,"operation":"describe"}),
            ),
        )
        .await;
        assert!(outcome.expect("bounded subprocess must finish").is_err());
    }
}

#[tokio::test]
async fn timeout_terminates_spawned_descendants_as_well_as_direct_child() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("descendant.pid");
    let mut fixture =
        command("sleep 60 & descendant=$!; printf '%s' \"$descendant\" > \"$1\"; wait");
    fixture.command.extend([
        "forge-process-fixture".into(),
        pid_file.to_str().unwrap().into(),
    ]);
    let outcome = tokio::time::timeout(
        Duration::from_secs(4),
        invoke(&fixture, json!({"api_version":1,"operation":"execute"})),
    )
    .await;
    assert!(outcome.expect("timeout cleanup must finish").is_err());
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .parse::<u32>()
        .unwrap();
    // A killed orphan can briefly remain a zombie awaiting the OS reaper. It
    // must never remain a live process after invoke returns.
    let mut live = true;
    for _ in 0..50 {
        live = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .is_some_and(|stat| {
                stat.rsplit_once(") ")
                    .and_then(|(_, tail)| tail.chars().next())
                    .is_some_and(|state| state != 'Z' && state != 'X')
            });
        if !live {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!live, "plugin timeout left a live descendant");
}

#[tokio::test]
async fn timeout_terminates_setsid_descendant() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("setsid-descendant.pid");
    let mut fixture =
        command("setsid sleep 60 & descendant=$!; printf '%s' \"$descendant\" > \"$1\"; wait");
    fixture.command.extend([
        "forge-process-fixture".into(),
        pid_file.to_str().unwrap().into(),
    ]);
    let outcome = tokio::time::timeout(
        Duration::from_secs(4),
        invoke(&fixture, json!({"api_version":1,"operation":"execute"})),
    )
    .await;
    assert!(outcome.expect("timeout cleanup must finish").is_err());
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .parse::<u32>()
        .unwrap();
    let mut live = true;
    for _ in 0..50 {
        live = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .is_some_and(|stat| {
                stat.rsplit_once(") ")
                    .and_then(|(_, tail)| tail.chars().next())
                    .is_some_and(|state| state != 'Z' && state != 'X')
            });
        if !live {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if live {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    assert!(!live, "plugin timeout left a live setsid descendant");
}

#[tokio::test]
async fn plugin_environment_is_an_explicit_allowlist_and_never_carries_forge_credentials() {
    use agent_forge::{
        core::Policy,
        settings::{Overrides, Settings},
    };
    // This test binary's other tests do not read these variables.
    unsafe {
        std::env::set_var(
            "FORGE_DATABASE_URL",
            format!(
                "{}://{}:{}@127.0.0.1/db",
                "postgresql", "leak", "fixture-password"
            ),
        );
        std::env::set_var("FORGE_TOKEN", "leaked-dashboard-token-value");
        std::env::set_var(
            "M3_DB_URL_VAR",
            format!("{}://{}:{}@127.0.0.1/db", "postgresql", "fixture", "viaenv"),
        );
        std::env::set_var("M3_PLUGIN_NEEDS", "granted-value");
        std::env::set_var("M3_NOT_GRANTED", "withheld-value");
    }
    let policy: Policy = serde_json::from_str(
        r#"{"home_id":"0123456789abcdef0123456789abcdef","actors":{},"database":{"url_env":"M3_DB_URL_VAR"}}"#,
    )
    .unwrap();
    let settings = Settings::resolve(
        "/nonexistent-m3-home".into(),
        &policy,
        &Overrides::default(),
        &|_| None,
    )
    .unwrap();
    settings.install();
    let print_env = "env | sort | sed 's/=.*//' | tr '\\n' ' ' > \"$0\".names; env > \"$0\".all; printf '%s' '{\"api_version\":1,\"output\":{}}'";
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("env");
    let config = PluginConfig {
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            print_env.into(),
            marker.to_string_lossy().into_owned(),
        ],
        timeout_seconds: 5,
        // The service's own credentials can be named, and are still withheld.
        env_passthrough: vec![
            "M3_PLUGIN_NEEDS".into(),
            "FORGE_DATABASE_URL".into(),
            "FORGE_TOKEN".into(),
            "M3_DB_URL_VAR".into(),
        ],
    };
    invoke(&config, json!({"api_version":1,"operation":"describe"}))
        .await
        .unwrap();
    let all = std::fs::read_to_string(directory.path().join("env.all")).unwrap();
    let names: std::collections::BTreeSet<&str> = all
        .lines()
        .filter_map(|l| l.split_once('=').map(|(n, _)| n))
        .collect();
    for secret in [
        "leak",
        "leaked-dashboard-token-value",
        "withheld-value",
        "viaenv",
    ] {
        assert!(
            !all.contains(secret),
            "{secret} reached the plugin: {names:?}"
        );
    }
    for forbidden in [
        "FORGE_DATABASE_URL",
        "FORGE_TOKEN",
        "M3_DB_URL_VAR",
        "M3_NOT_GRANTED",
    ] {
        assert!(!names.contains(forbidden), "{forbidden}");
    }
    assert!(all.contains("M3_PLUGIN_NEEDS=granted-value"));
    assert!(
        names.contains("PATH") && names.contains("LANG") && names.contains("FORGE_INVOCATION_ID")
    );
    // Only the explicit set (a shell may add PWD/SHLVL/_ of its own).
    let allowed = [
        "PATH",
        "LANG",
        "FORGE_INVOCATION_ID",
        "M3_PLUGIN_NEEDS",
        "PWD",
        "SHLVL",
        "_",
        "OLDPWD",
    ];
    assert!(names.iter().all(|n| allowed.contains(n)), "{names:?}");
    // Configuration naming a forbidden variable is refused at load time.
    for bad in [
        "FORGE_TOKEN",
        "FORGE_DATABASE_URL",
        "M3_DB_URL_VAR",
        "1BAD",
        "a b",
    ] {
        assert!(!agent_forge::plugin::passthrough_allowed(bad), "{bad}");
    }
    for (name, ok) in [
        ("FORGE_TOKEN", false),
        ("M3_DB_URL_VAR", false),
        ("PLUGIN_KEY", true),
    ] {
        let policy: Policy = serde_json::from_str(&format!(
            r#"{{"home_id":"0123456789abcdef0123456789abcdef","actors":{{}},"database":{{"url_env":"M3_DB_URL_VAR"}},"plugins":{{"plugin.x":{{"command":["/bin/true"],"env_passthrough":["{name}"]}}}}}}"#
        ))
        .unwrap();
        assert_eq!(
            agent_forge::core::validate_policy(
                std::path::Path::new("/nonexistent-m3-home"),
                &policy
            )
            .is_ok(),
            ok,
            "{name}"
        );
    }
    assert!(agent_forge::plugin::passthrough_allowed(
        "OPENAI_COMPAT_KEY"
    ));
}

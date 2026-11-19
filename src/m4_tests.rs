//! M4 tests: configuration refusals surfaced verbatim, private intermediate directories,
//! layout markers, backup notes, and the parity operations that need no native engine.
use crate::{
    core::{self},
    settings::{ConfigError, Paths, PathsConfig, Settings},
};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

fn message(error: anyhow::Error) -> String {
    error
        .downcast_ref::<ConfigError>()
        .map(|e| e.0.clone())
        .unwrap_or_else(|| panic!("not a ConfigError: {error:#}"))
}

#[test]
fn unknown_keys_in_paths_and_bad_types_are_named_without_values() {
    let home = tempfile::tempdir().unwrap();
    core::init(home.path()).unwrap();
    let file = crate::settings::config_file(home.path());
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    config["paths"] = json!({"statee": "/elsewhere"});
    std::fs::write(&file, config.to_string()).unwrap();
    let text = message(core::load_policy(home.path()).err().unwrap());
    assert!(text.contains("unknown field `statee`"), "{text}");
    assert!(text.contains("line"), "{text}");
    config["paths"] = json!({"state": 7});
    std::fs::write(&file, config.to_string()).unwrap();
    let text = message(core::load_policy(home.path()).err().unwrap());
    assert!(text.contains("wrong type or range"), "{text}");
    config["paths"] = json!({});
    config["server"] = json!({"port": "super-secret-value"});
    std::fs::write(&file, config.to_string()).unwrap();
    let text = message(core::load_policy(home.path()).err().unwrap());
    assert!(!text.contains("super-secret-value"), "{text}");
    std::fs::write(&file, "{ not json").unwrap();
    assert!(message(core::load_policy(home.path()).err().unwrap()).contains("not valid JSON"));
}

#[test]
fn weak_actor_tokens_and_bad_models_are_refused_with_a_reason() {
    let home = tempfile::tempdir().unwrap();
    core::init(home.path()).unwrap();
    let mut policy = core::load_policy(home.path()).unwrap();
    policy.actors.get_mut("operator").unwrap().token = "short".into();
    let text = message(core::validate_policy(home.path(), &policy).unwrap_err());
    assert!(
        text.contains("operator") && text.contains("high-entropy"),
        "{text}"
    );
    let model = core::Model {
        base_url: "ftp://x".into(),
        model: "m".into(),
        key_env: None,
    };
    assert!(message(core::validate_model("m1", &model).unwrap_err()).contains("m1"));
}

#[test]
fn a_non_loopback_server_host_is_refused_by_name() {
    let policy_for = |host: &str| -> core::Policy {
        let home = tempfile::tempdir().unwrap();
        core::init(home.path()).unwrap();
        let mut policy = core::load_policy(home.path()).unwrap();
        policy.server.host = Some(host.parse().unwrap());
        policy
    };
    let policy = policy_for("0.0.0.0");
    let home = tempfile::tempdir().unwrap();
    let error = Settings::resolve(
        home.path().to_path_buf(),
        &policy,
        &Default::default(),
        &|_| None,
    )
    .err()
    .unwrap();
    let text = message(error);
    assert!(
        text.contains("0.0.0.0") && text.contains("loopback"),
        "{text}"
    );
    let ok = policy_for("::1");
    Settings::resolve(home.path().to_path_buf(), &ok, &Default::default(), &|_| {
        None
    })
    .unwrap();
}

#[test]
fn a_symlinked_state_directory_is_refused_naming_the_setting() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&elsewhere, home.join("state")).unwrap();
    let error = Paths::defaults(&home).ensure_dirs().err().unwrap();
    let text = message(error);
    assert!(
        text.contains("paths.state") && text.contains("symbolic link"),
        "{text}"
    );
}

#[test]
fn intermediate_directories_outside_the_home_are_created_private() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let config = PathsConfig {
        logs: Some(root.path().join("outer/middle/logs")),
        ..Default::default()
    };
    let paths = Paths::resolve(&home, &config, &|_| None).unwrap();
    paths.ensure_dirs().unwrap();
    for dir in ["outer", "outer/middle", "outer/middle/logs"] {
        let mode = std::fs::metadata(root.path().join(dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{dir} is {mode:o}");
    }
}

#[test]
fn a_changed_state_location_on_an_existing_home_is_refused_with_recovery_steps() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let paths = Paths::defaults(&home);
    paths.ensure_dirs().unwrap();
    paths.check_layout().unwrap();
    paths.check_layout().unwrap();
    let moved = Paths::resolve(
        &home,
        &PathsConfig {
            state: Some(root.path().join("home/other-state")),
            ..Default::default()
        },
        &|_| None,
    )
    .unwrap();
    let text = message(moved.check_layout().err().unwrap());
    assert!(text.contains("paths.state changed"), "{text}");
    assert!(
        text.contains("home:state") && text.contains("home:other-state"),
        "{text}"
    );
}

#[test]
fn init_leaves_no_state_directory_or_owner_lock_behind() {
    let home = tempfile::tempdir().unwrap();
    core::init(home.path()).unwrap();
    assert!(!home.path().join("state").exists());
    assert!(crate::settings::config_file(home.path()).exists());
    // Idempotent on an existing valid config.
    core::init(home.path()).unwrap();
}

#[test]
fn backup_names_unarchived_locations_outside_the_home() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let config = PathsConfig {
        logs: Some(root.path().join("logs")),
        cache: Some(root.path().join("cache")),
        ..Default::default()
    };
    let paths = Paths::resolve(&home, &config, &|_| None).unwrap();
    let notes = crate::backup::outside_notes(&paths);
    assert_eq!(notes.len(), 2);
    assert!(notes[0].contains("paths.logs") && notes[0].contains("NOT in this backup"));
    assert!(crate::backup::outside_notes(&Paths::defaults(&home)).is_empty());
}

#[test]
fn backup_layout_refusal_for_state_outside_the_home_names_the_setting() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let config = PathsConfig {
        state: Some(root.path().join("elsewhere-state")),
        ..Default::default()
    };
    let paths = Paths::resolve(&home, &config, &|_| None).unwrap();
    let text = message(crate::backup::layout_check_for_test(&paths).unwrap_err());
    assert!(
        text.contains("paths.state") && text.contains("outside"),
        "{text}"
    );
}

fn core_with_runner() -> (tempfile::TempDir, std::sync::Arc<core::Core>) {
    use crate::dbos::admission::AdmissionStore;
    let home = tempfile::tempdir().unwrap();
    core::init(home.path()).unwrap();
    let mut policy = core::load_policy(home.path()).unwrap();
    policy.runners.insert(
        "codex-t".into(),
        serde_json::from_value(json!({"adapter":"codex","executable":"/usr/bin/true","permission_mode":"workspace-write"})).unwrap(),
    );
    policy.actors.insert(
        "reader".into(),
        core::Actor {
            token: "r".repeat(32),
            capabilities: vec!["read".into()],
            types: vec![],
        },
    );
    let config = Settings::for_tests(&policy)
        .runtime_config(&policy)
        .unwrap();
    let store = std::sync::Arc::new(
        AdmissionStore::initialize(&Paths::defaults(home.path()), &policy.home_id).unwrap(),
    );
    let core = core::Core::new(
        Paths::defaults(home.path()),
        policy,
        config,
        store,
        Default::default(),
    )
    .unwrap();
    (home, core)
}

#[test]
fn subagent_definitions_are_fixed_per_shape_and_carry_the_prompt_as_a_parameter() {
    let (id, def) = core::Core::subagent_definition("codex-t", "read-only", None).unwrap();
    assert_eq!(id, "subagent.codex-t.read-only");
    assert_eq!(def["type"], "harness");
    assert_eq!(def["input"]["prompt"], "${/params/prompt}");
    assert_eq!(def["input"]["profile"], "codex-t");
    // A custom schema changes the identity by digest; the same schema is the same identity.
    let schema = json!({"type":"object","properties":{"n":{"type":"integer"}},"required":["n"]});
    let (a, _) = core::Core::subagent_definition("codex-t", "read-only", Some(&schema)).unwrap();
    let (b, _) = core::Core::subagent_definition("codex-t", "read-only", Some(&schema)).unwrap();
    assert_eq!(a, b);
    assert_ne!(a, id);
    assert!(core::Core::subagent_definition(&"p".repeat(130), "read-only", None).is_err());
}

#[tokio::test]
async fn subagent_spawn_checks_authority_and_profile_before_admitting_anything() {
    let (_home, core) = core_with_runner();
    let err = |e: anyhow::Error| e.to_string();
    // A reader lacks execute: refused before any registration.
    let denied = core
        .subagent_spawn("reader", sp("codex-t", "hi", None))
        .await;
    assert!(err(denied.unwrap_err()).contains("outside actor authority"));
    assert!(core.work("operator", "subagent.codex-t.read-only").is_err());
    // Unknown profile, bad ceiling, empty prompt: Invalid, nothing registered.
    for (profile, prompt, perms, expect) in [
        ("missing", "hi", None, "runner profile is not configured"),
        ("codex-t", "hi", Some("root"), "permissions must be"),
        ("codex-t", "  ", None, "prompt is required"),
    ] {
        let e = core
            .subagent_spawn("operator", sp(profile, prompt, perms))
            .await;
        assert!(err(e.unwrap_err()).contains(expect), "{expect}");
    }
    assert!(core.work("operator", "subagent.codex-t.read-only").is_err());
    // A valid spawn registers the fixed definition, then needs the launched executor.
    let e = core
        .subagent_spawn("operator", sp("codex-t", "hi", None))
        .await;
    assert!(e.is_err());
    let work = core.work("operator", "subagent.codex-t.read-only").unwrap();
    assert_eq!(work["definition"]["type"], "harness");
    // Poll and cancel refuse runs that are not subagent runs (and unknown runs).
    assert!(core.subagent_poll("operator", "nope", None).await.is_err());
    assert!(core.subagent_cancel("operator", "nope").await.is_err());
}

fn sp<'a>(
    profile: &'a str,
    prompt: &'a str,
    permissions: Option<&'a str>,
) -> crate::subagents::Spawn<'a> {
    crate::subagents::Spawn {
        profile,
        prompt,
        instructions: None,
        key: "k1",
        permissions,
        schema: None,
    }
}

#[tokio::test]
async fn authority_errors_name_the_missing_capability_or_type_and_point_to_doctor() {
    let (_home, core) = core_with_runner();
    let text = core
        .permit("reader", "register", None)
        .unwrap_err()
        .to_string();
    assert!(
        text.contains("outside actor authority")
            && text.contains("capability `register`")
            && text.contains("agent-forge doctor"),
        "{text}"
    );
    let text = core
        .permit("operator", "execute", Some("plugin.nothing"))
        .unwrap_err()
        .to_string();
    assert!(text.contains("work type `plugin.nothing`"), "{text}");
    // Still the typed 403 cause.
    assert!(
        core.permit("reader", "execute", None)
            .unwrap_err()
            .is::<core::Denied>()
    );
}

#[tokio::test]
async fn provider_set_refuses_a_literal_secret_in_runner_env() {
    let (_home, core) = core_with_runner();
    let bad = json!({"adapter":"codex","executable":"/usr/bin/true","permission_mode":"read-only","env":{"MY_VAR":format!("sk-{}", ["abcdef", &"synthetic".repeat(4)].concat())}});
    let text = core
        .provider_set("operator", "runner", "leaky", &bad)
        .unwrap_err()
        .to_string();
    assert!(
        text.contains("literal secret")
            && text.contains("env_passthrough")
            && text.contains("MY_VAR"),
        "{text}"
    );
    assert!(!text.contains("sk-abcdef"), "{text}");
    let good = json!({"adapter":"codex","executable":"/usr/bin/true","permission_mode":"read-only","env":{"LANG":"C.UTF-8"},"env_passthrough":["OPENAI_API_KEY"]});
    core.provider_set("operator", "runner", "fine", &good)
        .unwrap();
}

#[test]
fn redaction_covers_actor_tokens_and_token_shaped_strings_in_inspect_projections() {
    let (_home, core) = core_with_runner();
    let token = core.policy.actors["operator"].token.clone();
    let run = json!({"output":{"echo":format!("token {token} and sk-{}", ["abcdef", &"synthetic".repeat(4)].concat())},"steps":[{"output":format!("x {token}")}]});
    let shown = core.redact_value(&run).to_string();
    assert!(
        !shown.contains(&token) && !shown.contains("sk-abcdef"),
        "{shown}"
    );
    assert!(shown.contains("[redacted]"));
}

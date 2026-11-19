//! Lifecycle and surface tests for installed plugins, against the real test-only fixture
//! plugin in `tests/fixtures/demo-plugin` (a real subprocess speaking protocol 2). Schedules
//! need a live DBOS client, so these tests drop the fixture job's schedule; the live proof
//! (scripts/qualify_plugins.py) covers schedules, DBOS runs and restarts.
use crate::{
    core::{self, Actor, Core},
    dbos::admission::AdmissionStore,
    plugin_manifest::{self as manifest, Manifest},
    plugin_registry::{CrashAt, PluginError},
    settings::Paths,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

const TYPE: &str = "plugin.demo-fixture";
type Files = BTreeMap<String, Vec<u8>>;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/demo-plugin")
}
fn read_tree(root: &Path) -> Files {
    fn visit(root: &Path, dir: &Path, out: &mut Files) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_str().unwrap().into(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut files = Files::new();
    visit(root, root, &mut files);
    files
}
/// Copy the fixture to `dst`, let `mutate` change files and manifest, then (when
/// `redigest`) recompute every declared digest so the result is internally consistent.
fn build_plugin(dst: &Path, redigest: bool, mutate: impl FnOnce(&mut Files, &mut Value)) {
    let mut files = read_tree(&fixture_dir());
    let mut manifest: Value =
        serde_json::from_slice(&files.remove(manifest::MANIFEST_FILE).unwrap()).unwrap();
    mutate(&mut files, &mut manifest);
    if redigest {
        for resource in manifest["resource_declarations"].as_array_mut().unwrap() {
            let bytes = &files[resource["path"].as_str().unwrap()];
            resource["digest"] = json!(manifest::digest_text(bytes));
        }
    }
    files.insert(
        manifest::MANIFEST_FILE.into(),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    );
    for (relative, bytes) in files {
        let path = dst.join(&relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let mode = if relative.starts_with("bin/") {
            0o755
        } else {
            0o644
        };
        std::os::unix::fs::PermissionsExt::set_mode(
            &mut std::fs::metadata(&path).unwrap().permissions(),
            mode,
        );
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(mode))
            .unwrap();
    }
}
/// The fixture without its schedule (no DBOS client in unit tests).
fn unscheduled(files: &mut Files, _manifest: &mut Value) {
    let mut job: Value = serde_json::from_slice(&files["jobs/daily.json"]).unwrap();
    job.as_object_mut().unwrap().remove("schedule");
    files.insert("jobs/daily.json".into(), serde_json::to_vec(&job).unwrap());
}

/// The fixture under another id (its work types follow), without schedules.
fn renamed(id: &'static str) -> impl FnOnce(&mut Files, &mut Value) {
    move |files, manifest| {
        unscheduled(files, manifest);
        manifest["id"] = json!(id);
        for path in ["jobs/daily.json", "jobs/slow.json", "flows/pipeline.json"] {
            let text = String::from_utf8(files[path].clone()).unwrap();
            files.insert(
                path.into(),
                text.replace("plugin.demo-fixture", &format!("plugin.{id}"))
                    .into_bytes(),
            );
        }
    }
}

/// The fixture's `cli:hello` contribution.
fn cli_hello(manifest: &mut Value) -> &mut Value {
    manifest["contributions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|c| c["kind"] == "cli" && c["id"] == "hello")
        .unwrap()
}

struct Env {
    home: tempfile::TempDir,
    core: Arc<Core>,
    source: tempfile::TempDir,
}
fn policy_for(home: &Path) -> core::Policy {
    let mut policy = core::load_policy(home).unwrap();
    let operator = policy.actors.get_mut("operator").unwrap();
    operator.types.extend(
        [
            TYPE,
            "plugin.base-plugin",
            "plugin.rival-plugin",
            "plugin.b-plugin",
        ]
        .map(String::from),
    );
    policy.actors.insert(
        "viewer".into(),
        Actor {
            token: "v".repeat(32),
            capabilities: vec!["read".into()],
            types: vec![TYPE.into()],
        },
    );
    policy.actors.insert(
        "stranger".into(),
        Actor {
            token: "s".repeat(32),
            capabilities: vec!["read".into(), "execute".into()],
            types: vec![],
        },
    );
    policy
}
fn core_at(home: &Path) -> Arc<Core> {
    let policy = policy_for(home);
    let config = crate::settings::Settings::for_tests(&policy)
        .runtime_config(&policy)
        .unwrap();
    let store =
        Arc::new(AdmissionStore::initialize(&Paths::defaults(home), &policy.home_id).unwrap());
    Core::new(
        Paths::defaults(home),
        policy,
        config,
        store,
        BTreeMap::new(),
    )
    .unwrap()
}
fn env() -> Env {
    let home = tempfile::tempdir().unwrap();
    core::init(home.path()).unwrap();
    let core = core_at(home.path());
    let source = tempfile::tempdir().unwrap();
    build_plugin(source.path(), true, unscheduled);
    Env { home, core, source }
}
impl Env {
    fn src(&self) -> String {
        self.source.path().to_str().unwrap().into()
    }
    async fn install(&self) -> Value {
        self.core
            .plugin_install("operator", &self.src(), false)
            .await
            .unwrap()
    }
    async fn enabled(&self) -> Value {
        self.install().await;
        self.core
            .plugin_enable("operator", "demo-fixture")
            .await
            .unwrap()
    }
    fn release(&self) -> PathBuf {
        Paths::defaults(self.home.path()).plugin_release("demo-fixture", "1.0.0")
    }
    fn staging_entries(&self) -> usize {
        std::fs::read_dir(Paths::defaults(self.home.path()).plugin_staging())
            .map(|d| d.count())
            .unwrap_or(0)
    }
}
fn message(error: anyhow::Error) -> String {
    error.to_string()
}

#[test]
fn committed_fixture_digests_are_current() {
    let files = read_tree(&fixture_dir());
    let m = Manifest::parse(&files[manifest::MANIFEST_FILE]).unwrap();
    for resource in &m.resource_declarations {
        assert_eq!(
            manifest::digest_text(&files[&resource.path]),
            resource.digest,
            "stale digest for {}: run tests/fixtures/regenerate_demo_plugin.py",
            resource.identity()
        );
    }
    // One contribution of every MVP kind.
    for kind in [
        manifest::Kind::Job,
        manifest::Kind::Workflow,
        manifest::Kind::Cli,
        manifest::Kind::Api,
        manifest::Kind::Mcp,
        manifest::Kind::Dashboard,
        manifest::Kind::Assets,
    ] {
        assert!(m.of_kind(kind).next().is_some(), "{kind:?}");
    }
    // The fixture job has a cron schedule and is disabled by default.
    let job: Value = serde_json::from_slice(&files["jobs/daily.json"]).unwrap();
    assert_eq!(job["enabled"], false);
    assert_eq!(job["schedule"]["cron"], "* * * * *");
}

#[tokio::test]
async fn install_is_disabled_by_default_and_exposes_nothing() {
    let e = env();
    let view = e.install().await;
    assert_eq!(view["state"], "disabled");
    assert_eq!(view["enabled"], false);
    assert!(e.release().join("bin/demo-plugin").is_file());
    // Not a type, not dispatchable, no work, no tools, no assets, no API.
    assert!(e.core.plugin(TYPE).is_none());
    assert!(e.core.types().get(TYPE).is_none());
    assert!(
        e.core
            .work("operator", "plugin.demo-fixture.daily")
            .is_err()
    );
    assert_eq!(e.core.plugin_tools("operator").unwrap(), json!([]));
    assert!(
        e.core
            .plugin_asset("demo-fixture", "ui/page.js")
            .await
            .is_err()
    );
    let reply = e
        .core
        .plugin_api("operator", "demo-fixture", "GET", "status", "", b"")
        .await
        .unwrap();
    assert_eq!(reply.status, 503);
    assert_eq!(e.staging_entries(), 0);
    // A second install of the same id is refused.
    let again = e.core.plugin_install("operator", &e.src(), false).await;
    assert!(message(again.unwrap_err()).contains("already installed"));
    // Dry run validates without installing.
    let other = tempfile::tempdir().unwrap();
    build_plugin(other.path(), true, |_, m| m["id"] = json!("other-demo"));
    let result = e
        .core
        .plugin_install("operator", other.path().to_str().unwrap(), true)
        .await
        .unwrap();
    assert_eq!(result["installed"], false);
    assert_eq!(e.core.plugins_view().as_array().unwrap().len(), 1);
    assert_eq!(e.staging_entries(), 0);
}

#[tokio::test]
async fn install_requires_the_plugins_capability() {
    let e = env();
    for actor in ["viewer", "stranger"] {
        let denied = e.core.plugin_install(actor, &e.src(), false).await;
        assert!(message(denied.unwrap_err()).contains("outside actor authority"));
    }
    assert!(
        e.core
            .plugin_enable("viewer", "demo-fixture")
            .await
            .is_err()
    );
    assert!(
        e.core
            .plugin_disable("viewer", "demo-fixture")
            .await
            .is_err()
    );
    assert!(
        e.core
            .plugin_uninstall("viewer", "demo-fixture")
            .await
            .is_err()
    );
}

fn refused(mutated: impl FnOnce(&mut Files, &mut Value), redigest: bool) -> String {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let e = env();
        let dir = tempfile::tempdir().unwrap();
        build_plugin(dir.path(), redigest, mutated);
        let error = e
            .core
            .plugin_install("operator", dir.path().to_str().unwrap(), false)
            .await
            .unwrap_err();
        // Nothing half-installed: no row, no release, no leftover staging.
        assert!(e.core.plugins_view().as_array().unwrap().is_empty());
        assert!(!e.release().exists(), "refused install left a release");
        assert_eq!(e.staging_entries(), 0, "refused install left staging");
        error.to_string()
    })
}

#[test]
fn install_refuses_bad_digests_undeclared_files_and_bad_manifests() {
    // Tampered bytes after the manifest's digests were computed.
    let tampered = refused(
        |f, _| {
            f.get_mut("ui/page.js").unwrap().extend_from_slice(b"//x");
        },
        false,
    );
    assert!(
        tampered.contains("digest mismatch for asset:page-js"),
        "{tampered}"
    );
    // A malformed digest string.
    let malformed = refused(
        |_, m| m["resource_declarations"][0]["digest"] = json!("sha256:nope"),
        false,
    );
    assert!(malformed.contains("digest"), "{malformed}");
    // An undeclared executable or asset riding along.
    let extra = refused(
        |f, _| {
            f.insert("bin/backdoor".into(), b"#!/bin/sh\n".to_vec());
        },
        true,
    );
    assert!(
        extra.contains("undeclared file in plugin: bin/backdoor"),
        "{extra}"
    );
    // A declared resource that is missing.
    let missing = refused(
        |f, m| {
            f.remove("ui/page.css");
            m["resource_declarations"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|r| r["id"] == "page-css")
                .unwrap()["digest"] = json!(format!("sha256:{}", "0".repeat(64)));
        },
        false,
    );
    assert!(missing.contains("missing"), "{missing}");
    // Unknown contribution kind, forbidden metadata key, unknown manifest field.
    assert!(
        refused(|_, m| m["contributions"][0]["kind"] = json!("agents"), true)
            .contains("unknown contribution kind")
    );
    assert!(
        refused(|_, m| cli_hello(m)["metadata"]["hook"] = json!("x"), true).contains("forbidden")
    );
    assert!(refused(|_, m| m["surprise"] = json!(1), true).contains("strict"));
    // Duplicate contribution (ownership collision inside the plugin).
    let duplicate = refused(
        |_, m| {
            let c = cli_hello(m).clone();
            m["contributions"].as_array_mut().unwrap().push(c);
        },
        true,
    );
    assert!(duplicate.contains("ownership collision"), "{duplicate}");
    // The executable must be executable.
    let not_executable = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let e = env();
            let dir = tempfile::tempdir().unwrap();
            build_plugin(dir.path(), true, |_, _| {});
            std::fs::set_permissions(
                dir.path().join("bin/demo-plugin"),
                std::os::unix::fs::PermissionsExt::from_mode(0o644),
            )
            .unwrap();
            e.core
                .plugin_install("operator", dir.path().to_str().unwrap(), false)
                .await
                .unwrap_err()
                .to_string()
        })
    };
    assert!(not_executable.contains("execute bit"), "{not_executable}");
}

#[tokio::test]
async fn install_refuses_symlinks_and_relative_sources() {
    let e = env();
    let dir = tempfile::tempdir().unwrap();
    build_plugin(dir.path(), true, |_, _| {});
    std::os::unix::fs::symlink("/etc/passwd", dir.path().join("ui/link.js")).unwrap();
    let error = e
        .core
        .plugin_install("operator", dir.path().to_str().unwrap(), false)
        .await
        .unwrap_err();
    assert!(message(error).contains("no symlinks"));
    assert!(
        e.core
            .plugin_install("operator", "relative/dir", false)
            .await
            .is_err()
    );
    assert_eq!(e.staging_entries(), 0);
}

#[tokio::test]
async fn ownership_collisions_and_dependency_cycles_are_refused() {
    let e = env();
    // A configured plugin type of the same name collides.
    let mut policy = policy_for(e.home.path());
    policy.plugins.insert(
        TYPE.into(),
        serde_json::from_value(json!({"command":["/bin/true"]})).unwrap(),
    );
    let config = crate::settings::Settings::for_tests(&policy)
        .runtime_config(&policy)
        .unwrap();
    let store = Arc::new(
        AdmissionStore::initialize(&Paths::defaults(e.home.path()), &policy.home_id).unwrap(),
    );
    let collided = Core::new(
        Paths::defaults(e.home.path()),
        policy,
        config,
        store,
        BTreeMap::new(),
    )
    .unwrap();
    let error = collided
        .plugin_install("operator", &e.src(), false)
        .await
        .unwrap_err();
    assert!(message(error).contains("ownership collision"));
    // A user work id in the plugin's namespace can't be registered, so seed one directly.
    e.core
        .store
        .write(|c| {
            c.execute(
                "INSERT INTO forge_work VALUES('plugin.demo-fixture.daily','operator','{}',0,0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let error = e
        .core
        .plugin_install("operator", &e.src(), false)
        .await
        .unwrap_err();
    assert!(message(error).contains("work id plugin.demo-fixture.daily already exists"));
    e.core
        .store
        .write(|c| {
            c.execute("DELETE FROM forge_work", [])?;
            Ok(())
        })
        .unwrap();
    // Cycle: A (demo-fixture) requires b-plugin; installing b-plugin requiring A is a cycle.
    let a = tempfile::tempdir().unwrap();
    build_plugin(a.path(), true, |f, m| {
        unscheduled(f, m);
        m["requires"] = json!(["b-plugin"]);
    });
    e.core
        .plugin_install("operator", a.path().to_str().unwrap(), false)
        .await
        .unwrap();
    let b = tempfile::tempdir().unwrap();
    build_plugin(b.path(), true, |f, m| {
        unscheduled(f, m);
        m["id"] = json!("b-plugin");
        m["requires"] = json!(["demo-fixture"]);
    });
    let error = e
        .core
        .plugin_install("operator", b.path().to_str().unwrap(), false)
        .await
        .unwrap_err();
    assert!(message(error).contains("dependency cycle"));
}

#[tokio::test]
async fn enable_materializes_namespaced_work_and_disable_denies_every_surface() {
    let e = env();
    let view = e.enabled().await;
    assert_eq!(view["state"], "enabled");
    assert!(e.core.types().get(TYPE).is_some());
    // The job is materialized disabled (the manifest says enabled:false); the workflow too.
    let job = e
        .core
        .work("operator", "plugin.demo-fixture.daily")
        .unwrap();
    assert_eq!(
        (job["enabled"].as_bool(), job["definition"]["type"].as_str()),
        (Some(false), Some(TYPE))
    );
    assert!(
        e.core
            .work("operator", "plugin.demo-fixture.pipeline")
            .is_ok()
    );
    // Users cannot register into the reserved namespace.
    let reserved = e.core.register(
        "operator",
        "plugin.demo-fixture.mine",
        json!({"type":"echo","input":{"text":"x"}}),
    );
    assert!(reserved.is_err());
    // An operator enables the job explicitly; it then passes the admission gate.
    e.core
        .enable("operator", "plugin.demo-fixture.daily", true)
        .unwrap();
    assert!(
        e.core
            .admitted("operator", "plugin.demo-fixture.daily")
            .is_ok()
    );
    // Every surface answers while enabled.
    assert_eq!(
        e.core
            .plugin_tools("operator")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let hello = e
        .core
        .plugin_run(
            "operator",
            "demo-fixture",
            &["hello".into(), "world".into()],
        )
        .await
        .unwrap();
    assert_eq!(
        (hello["stdout"].as_str(), hello["status"].as_u64()),
        (Some("hello world\n"), Some(0))
    );
    // --- disable ---
    let disabled = e
        .core
        .plugin_disable("operator", "demo-fixture")
        .await
        .unwrap();
    assert_eq!(disabled["state"], "disabled");
    // Disabled-cannot-dispatch, enforced at dispatch (admission), not only in the UI.
    let denied = e.core.admitted("operator", "plugin.demo-fixture.daily");
    assert!(message(denied.unwrap_err()).contains("not enabled"));
    assert_eq!(
        e.core
            .work("operator", "plugin.demo-fixture.daily")
            .unwrap()["enabled"],
        false,
        "materialized work is deactivated"
    );
    assert!(e.core.plugin(TYPE).is_none());
    assert!(e.core.types().get(TYPE).is_none());
    // A user workflow that names the disabled plugin type cannot be bound for dispatch.
    let flow = json!({"type":"workflow","input":{"steps":[{"id":"a","action":{"type":TYPE,"input":{"text":"x"}}}]}});
    assert!(e.core.action_hash("operator", &flow).is_err());
    assert!(
        e.core
            .check_definition("operator", &json!({"type":TYPE,"input":{"text":"x"}}))
            .is_err()
    );
    // CLI returns the declared denial; API 503; MCP tool absent; assets 404.
    let cli = e
        .core
        .plugin_run("operator", "demo-fixture", &["hello".into()])
        .await
        .unwrap();
    assert_eq!(
        (cli["denied"].as_bool(), cli["status"].as_u64()),
        (Some(true), Some(3))
    );
    assert_eq!(cli["message"], "demo-fixture is installed but disabled");
    let fail = e
        .core
        .plugin_run("operator", "demo-fixture", &["fail".into()])
        .await
        .unwrap();
    assert_eq!(
        fail["message"], "plugin is installed but disabled",
        "default denial when none is declared"
    );
    let api = e
        .core
        .plugin_api("operator", "demo-fixture", "GET", "status", "", b"")
        .await
        .unwrap();
    assert_eq!(api.status, 503);
    assert_eq!(e.core.plugin_tools("operator").unwrap(), json!([]));
    assert!(
        e.core
            .plugin_mcp("operator", "plugin_demo-fixture__echo", json!({"text":"x"}))
            .await
            .is_err()
    );
    assert!(
        e.core
            .plugin_asset("demo-fixture", "ui/page.js")
            .await
            .is_err()
    );
    // Re-enable resets the materialized job to the manifest default (disabled).
    e.core
        .plugin_enable("operator", "demo-fixture")
        .await
        .unwrap();
    assert_eq!(
        e.core
            .work("operator", "plugin.demo-fixture.daily")
            .unwrap()["enabled"],
        false
    );
}

#[tokio::test]
async fn disable_is_refused_while_a_direct_call_is_in_flight() {
    let e = env();
    e.enabled().await;
    let slow = e.source.path().join("slow");
    let _ = slow;
    // Hold a call open by taking a guard the way a CLI/API/MCP call does.
    let guard = e.core.hold_call_for_test("demo-fixture");
    let error = e
        .core
        .plugin_disable("operator", "demo-fixture")
        .await
        .unwrap_err();
    assert!(message(error).contains("in flight"));
    // The plugin is still enabled and dispatchable after the refusal.
    assert!(e.core.plugin(TYPE).is_some());
    drop(guard);
    assert!(
        e.core
            .plugin_disable("operator", "demo-fixture")
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn uninstall_requires_disabled_and_no_references_and_removes_the_release() {
    let e = env();
    e.enabled().await;
    let enabled = e.core.plugin_uninstall("operator", "demo-fixture").await;
    assert!(message(enabled.unwrap_err()).contains("disable the plugin"));
    // A user flow referencing the plugin blocks uninstall (and the type is checked while enabled).
    e.core.register("operator", "user-flow", json!({"type":"workflow","input":{"steps":[{"id":"a","action":{"type":TYPE,"input":{"text":"x"}}}]}})).unwrap();
    e.core
        .plugin_disable("operator", "demo-fixture")
        .await
        .unwrap();
    let blocked = e.core.plugin_uninstall("operator", "demo-fixture").await;
    assert!(message(blocked.unwrap_err()).contains("user-flow"));
    e.core.enable("operator", "user-flow", false).unwrap();
    // Still referenced (disabling the flow does not remove the reference); drop it for the test.
    e.core
        .store
        .write(|c| {
            c.execute("DELETE FROM forge_work WHERE id='user-flow'", [])?;
            Ok(())
        })
        .unwrap();
    let done = e
        .core
        .plugin_uninstall("operator", "demo-fixture")
        .await
        .unwrap();
    assert_eq!(done["uninstalled"], true);
    assert!(!e.release().exists());
    assert!(e.core.plugins_view().as_array().unwrap().is_empty());
    // Materialized work stays readable but cannot dispatch.
    assert!(e.core.work("operator", "plugin.demo-fixture.daily").is_ok());
    assert!(
        e.core
            .admitted("operator", "plugin.demo-fixture.daily")
            .is_err()
    );
    // Reinstalling adopts that retained work.
    e.install().await;
    e.core
        .plugin_enable("operator", "demo-fixture")
        .await
        .unwrap();
    assert!(e.core.work("operator", "plugin.demo-fixture.daily").is_ok());
}

#[tokio::test]
async fn requires_and_conflicts_are_enforced_on_enable_and_disable() {
    let e = env();
    let base = tempfile::tempdir().unwrap();
    build_plugin(base.path(), true, renamed("base-plugin"));
    let dependent = tempfile::tempdir().unwrap();
    build_plugin(dependent.path(), true, |f, m| {
        unscheduled(f, m);
        m["requires"] = json!(["base-plugin"]);
    });
    let rival = tempfile::tempdir().unwrap();
    build_plugin(rival.path(), true, |f, m| {
        renamed("rival-plugin")(f, m);
        m["conflicts"] = json!(["demo-fixture"]);
    });
    for dir in [&base, &dependent, &rival] {
        e.core
            .plugin_install("operator", dir.path().to_str().unwrap(), false)
            .await
            .unwrap();
    }
    // Needs base first.
    let early = e.core.plugin_enable("operator", "demo-fixture").await;
    assert!(message(early.unwrap_err()).contains("required plugin base-plugin is not enabled"));
    e.core
        .plugin_enable("operator", "base-plugin")
        .await
        .unwrap();
    e.core
        .plugin_enable("operator", "demo-fixture")
        .await
        .unwrap();
    // Cannot disable or uninstall base while a dependent is enabled/installed.
    assert!(
        message(
            e.core
                .plugin_disable("operator", "base-plugin")
                .await
                .unwrap_err()
        )
        .contains("requires base-plugin")
    );
    // The rival conflicts with the enabled plugin.
    let conflict = e.core.plugin_enable("operator", "rival-plugin").await;
    assert!(message(conflict.unwrap_err()).contains("conflict"));
    e.core
        .plugin_disable("operator", "demo-fixture")
        .await
        .unwrap();
    // The base must itself be disabled before it can be uninstalled.
    e.core
        .plugin_disable("operator", "base-plugin")
        .await
        .unwrap();
    assert!(
        message(
            e.core
                .plugin_uninstall("operator", "base-plugin")
                .await
                .unwrap_err()
        )
        .contains("requires base-plugin")
    );
}

#[tokio::test]
async fn enable_reverifies_the_release_bytes() {
    let e = env();
    e.install().await;
    let script = e.release().join("bin/demo-plugin");
    std::os::unix::fs::PermissionsExt::set_mode(
        &mut std::fs::metadata(&script).unwrap().permissions(),
        0o700,
    );
    std::fs::set_permissions(
        e.release(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(
        e.release().join("bin"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    std::fs::write(&script, "#!/bin/sh\necho hacked\n").unwrap();
    let error = e
        .core
        .plugin_enable("operator", "demo-fixture")
        .await
        .unwrap_err();
    assert!(message(error).contains("digest mismatch for script:main"));
    assert!(e.core.plugin(TYPE).is_none());
}

// ---- Crash safety ------------------------------------------------------------------
#[tokio::test]
async fn a_crash_after_staging_leaves_nothing_visible_and_a_retry_succeeds() {
    let e = env();
    let error = e
        .core
        .plugin_install_with("operator", &e.src(), false, Some(CrashAt::AfterStage))
        .await
        .unwrap_err();
    assert!(message(error).contains("simulated crash"));
    // Killed mid-install: staged files remain, but nothing is registered or enableable.
    assert_eq!(e.staging_entries(), 1);
    assert!(!e.release().exists());
    assert!(e.core.plugins_view().as_array().unwrap().is_empty());
    assert!(
        message(
            e.core
                .plugin_enable("operator", "demo-fixture")
                .await
                .unwrap_err()
        )
        .contains("not installed")
    );
    // Restart: recovery removes the debris.
    let restarted = core_at(e.home.path());
    restarted.plugins_startup().await.unwrap();
    assert_eq!(e.staging_entries(), 0);
    assert!(restarted.plugins_view().as_array().unwrap().is_empty());
    let view = restarted
        .plugin_install("operator", &e.src(), false)
        .await
        .unwrap();
    assert_eq!(view["state"], "disabled");
    restarted
        .plugin_enable("operator", "demo-fixture")
        .await
        .unwrap();
}

#[tokio::test]
async fn a_crash_after_the_rename_leaves_an_orphan_that_is_invisible_and_replaced() {
    let e = env();
    let error = e
        .core
        .plugin_install_with("operator", &e.src(), false, Some(CrashAt::AfterRename))
        .await
        .unwrap_err();
    assert!(message(error).contains("simulated crash"));
    // The release directory exists but no registry row does: not visible, not enableable.
    assert!(e.release().is_dir());
    assert!(e.core.plugins_view().as_array().unwrap().is_empty());
    assert!(
        message(
            e.core
                .plugin_enable("operator", "demo-fixture")
                .await
                .unwrap_err()
        )
        .contains("not installed")
    );
    // Retry WITHOUT a restart replaces the orphan and succeeds.
    let view = e
        .core
        .plugin_install("operator", &e.src(), false)
        .await
        .unwrap();
    assert_eq!(view["state"], "disabled");
    e.core
        .plugin_enable("operator", "demo-fixture")
        .await
        .unwrap();
    // And a restart after another crash removes the orphan before anything can use it.
    e.core
        .plugin_disable("operator", "demo-fixture")
        .await
        .unwrap();
    e.core
        .plugin_uninstall("operator", "demo-fixture")
        .await
        .unwrap();
    let _ = e
        .core
        .plugin_install_with("operator", &e.src(), false, Some(CrashAt::AfterRename))
        .await;
    assert!(e.release().is_dir());
    let restarted = core_at(e.home.path());
    restarted.plugins_startup().await.unwrap();
    assert!(!e.release().exists(), "startup removed the orphan release");
    assert!(restarted.plugins_view().as_array().unwrap().is_empty());
}

#[tokio::test]
async fn enabled_state_and_work_survive_a_restart_and_broken_releases_are_not_enableable() {
    let e = env();
    e.enabled().await;
    let restarted = core_at(e.home.path());
    restarted.plugins_startup().await.unwrap();
    assert!(
        restarted.plugin(TYPE).is_some(),
        "enabled plugin is active again after restart"
    );
    assert!(
        restarted
            .work("operator", "plugin.demo-fixture.daily")
            .is_ok()
    );
    assert!(
        restarted
            .admitted("operator", "plugin.demo-fixture.daily")
            .is_err(),
        "job stays disabled"
    );
    // A release that no longer matches its digests comes back broken, not enabled.
    let script = e.release().join("bin/demo-plugin");
    std::fs::set_permissions(
        e.release().join("bin"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    std::fs::write(&script, "#!/bin/sh\necho tampered\n").unwrap();
    let after = core_at(e.home.path());
    after.plugins_startup().await.unwrap();
    assert!(after.plugin(TYPE).is_none());
    let view = after.plugins_view();
    assert_eq!(view[0]["state"], "broken");
    assert!(
        view[0]["problem"]
            .as_str()
            .unwrap()
            .contains("digest mismatch")
    );
    assert!(
        message(
            after
                .plugin_enable("operator", "demo-fixture")
                .await
                .unwrap_err()
        )
        .contains("cannot be enabled")
    );
}

// ---- Protocol 2 surfaces --------------------------------------------------------------
#[tokio::test]
async fn cli_runs_only_declared_commands_with_the_plugins_exit_status() {
    let e = env();
    e.enabled().await;
    let undeclared = e
        .core
        .plugin_run("operator", "demo-fixture", &["rm".into()])
        .await;
    assert!(message(undeclared.unwrap_err()).contains("not declared"));
    let fail = e
        .core
        .plugin_run("operator", "demo-fixture", &["fail".into()])
        .await
        .unwrap();
    assert_eq!(
        (fail["status"].as_u64(), fail["stderr"].as_str()),
        (Some(7), Some("boom\n"))
    );
    // Hyphen-leading arguments are data after the declared path.
    let args = e
        .core
        .plugin_run(
            "operator",
            "demo-fixture",
            &["hello".into(), "--flag".into(), "x y".into()],
        )
        .await
        .unwrap();
    assert_eq!(args["stdout"], "hello --flag x y\n");
    // Authority: the actor needs execute and the plugin's type.
    assert!(
        e.core
            .plugin_run("viewer", "demo-fixture", &["hello".into()])
            .await
            .is_err()
    );
    assert!(
        e.core
            .plugin_run("stranger", "demo-fixture", &["hello".into()])
            .await
            .is_err()
    );
    let unknown = e
        .core
        .plugin_run("operator", "nope", &["hello".into()])
        .await;
    assert!(message(unknown.unwrap_err()).contains("not installed"));
}

#[tokio::test]
async fn api_routes_enforce_declared_methods_capabilities_limits_and_header_allow_lists() {
    let e = env();
    e.enabled().await;
    let call = |actor: &'static str,
                method: &'static str,
                path: &'static str,
                query: &'static str,
                body: Vec<u8>| {
        let core = e.core.clone();
        async move {
            core.plugin_api(actor, "demo-fixture", method, path, query, &body)
                .await
        }
    };
    // GET /status needs only `read` (plus the plugin type): the viewer may call it.
    let ok = call("viewer", "GET", "status", "a=1&b=%20x", vec![])
        .await
        .unwrap();
    assert_eq!(ok.status, 200);
    let body: Value = serde_json::from_slice(&ok.body).unwrap();
    assert_eq!(
        (
            body["principal"].as_str(),
            body["query"]["a"].as_str(),
            body["query"]["b"].as_str()
        ),
        (Some("viewer"), Some("1"), Some(" x"))
    );
    assert!(ok.headers.iter().any(|(n, v)| n == "etag" && v == "v1"));
    // POST /echo needs `execute`: the read-only viewer is denied, the operator is not.
    assert!(
        call("viewer", "POST", "echo", "", b"{}".to_vec())
            .await
            .is_err()
    );
    let created = call("operator", "POST", "echo", "", br#"{"hi":1}"#.to_vec())
        .await
        .unwrap();
    assert_eq!(created.status, 201);
    // An actor without the plugin's type is denied even for a read route.
    assert!(call("stranger", "GET", "status", "", vec![]).await.is_err());
    // Undeclared route -> 404; declared path, other method -> 405.
    assert_eq!(
        call("operator", "GET", "secret", "", vec![])
            .await
            .unwrap()
            .status,
        404
    );
    assert_eq!(
        call("operator", "DELETE", "status", "", vec![])
            .await
            .unwrap()
            .status,
        405
    );
    assert_eq!(
        call("operator", "GET", "../status", "", vec![])
            .await
            .unwrap()
            .status,
        404
    );
    // Body limit (1024 bytes in the manifest) and JSON-only bodies.
    assert_eq!(
        call("operator", "POST", "echo", "", vec![b' '; 2000])
            .await
            .unwrap()
            .status,
        413
    );
    assert_eq!(
        call("operator", "POST", "echo", "", b"not json".to_vec())
            .await
            .unwrap()
            .status,
        400
    );
    // A plugin may not return HTML or set arbitrary headers: both become a 502.
    assert_eq!(
        call("operator", "GET", "html", "", vec![])
            .await
            .unwrap()
            .status,
        502
    );
    assert_eq!(
        call("operator", "GET", "header-injection", "", vec![])
            .await
            .unwrap()
            .status,
        502
    );
}

#[tokio::test]
async fn mcp_tools_are_namespaced_listed_only_when_permitted_and_validate_arguments() {
    let e = env();
    e.enabled().await;
    let tools = e.core.plugin_tools("operator").unwrap();
    assert_eq!(tools[0]["name"], "plugin_demo-fixture__echo");
    assert_eq!(tools[0]["read_only"], true);
    // Not listed for an actor that may not execute the plugin's type.
    assert_eq!(e.core.plugin_tools("stranger").unwrap(), json!([]));
    assert_eq!(
        e.core.plugin_tools("viewer").unwrap(),
        json!([]),
        "viewer has no execute capability"
    );
    let reply = e
        .core
        .plugin_mcp(
            "operator",
            "plugin_demo-fixture__echo",
            json!({"text":"hi"}),
        )
        .await
        .unwrap();
    assert_eq!(reply["output"]["echo"], "hi");
    for bad in [
        json!({}),
        json!({"text":1}),
        json!({"text":"x","extra":true}),
    ] {
        assert!(
            e.core
                .plugin_mcp("operator", "plugin_demo-fixture__echo", bad)
                .await
                .is_err()
        );
    }
    assert!(
        e.core
            .plugin_mcp("operator", "forge_snapshot", json!({}))
            .await
            .is_err()
    );
    assert!(
        e.core
            .plugin_mcp("stranger", "plugin_demo-fixture__echo", json!({"text":"x"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn assets_are_served_only_from_digest_verified_release_bytes() {
    let e = env();
    e.enabled().await;
    let (kind, bytes) = e
        .core
        .plugin_asset("demo-fixture", "ui/page.js")
        .await
        .unwrap();
    assert_eq!(kind, "text/javascript; charset=utf-8");
    assert!(
        String::from_utf8(bytes)
            .unwrap()
            .contains("export async function mount")
    );
    assert_eq!(
        e.core
            .plugin_asset("demo-fixture", "ui/page.css")
            .await
            .unwrap()
            .0,
        "text/css; charset=utf-8"
    );
    // Only declared assets: not the script, the manifest, or a traversal.
    for path in [
        "bin/demo-plugin",
        "manifest.json",
        "jobs/daily.json",
        "../manifest.json",
        "ui/missing.js",
    ] {
        assert!(
            e.core.plugin_asset("demo-fixture", path).await.is_err(),
            "{path}"
        );
    }
    // Tampered release bytes are refused at request time.
    let page = e.release().join("ui/page.js");
    std::fs::set_permissions(
        e.release().join("ui"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(&page, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    std::fs::write(&page, "export const mount = () => alert(1);").unwrap();
    let tampered = e.core.plugin_asset("demo-fixture", "ui/page.js").await;
    assert!(message(tampered.unwrap_err()).contains("digest"));
}

#[tokio::test]
async fn health_check_reverifies_and_redescribes() {
    let e = env();
    e.enabled().await;
    let shown = e
        .core
        .plugin_show("operator", "demo-fixture", true)
        .await
        .unwrap();
    assert_eq!(shown["health"]["release_verified"], true);
    assert_eq!(shown["health"]["describe_ok"], true);
    assert!(
        e.core
            .plugin_show("operator", "ghost", false)
            .await
            .is_err()
    );
}

// ---- Protocol versions and the legacy configured-plugin path ----------------------------
#[tokio::test]
async fn protocol_one_configured_plugins_keep_working_unchanged() {
    use crate::plugin::{Plugin, PluginConfig};
    let v1 = Plugin {
        config: PluginConfig {
            command: vec!["/bin/true".into()],
            timeout_seconds: 5,
            env_passthrough: vec![],
        },
        description: "legacy".into(),
        schema: json!({"type":"object"}),
        api_version: 1,
    };
    let request = v1.execute_request(&json!({"a":1}), "run", "key");
    assert_eq!(request["api_version"], 1);
    assert_eq!(request["operation"], "execute");
    // A v1 reply is accepted for a v1 request and refused for a v2 request, and vice versa.
    let config = PluginConfig {
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            "read -r request; printf '{\"api_version\":1,\"output\":{}}'".into(),
        ],
        timeout_seconds: 5,
        env_passthrough: vec![],
    };
    assert!(
        crate::plugin::invoke(&config, json!({"api_version":1,"operation":"execute"}))
            .await
            .is_ok()
    );
    assert!(
        crate::plugin::invoke(&config, json!({"api_version":2,"operation":"execute"}))
            .await
            .is_err()
    );
    assert!(
        crate::plugin::invoke(&config, json!({"api_version":3,"operation":"execute"}))
            .await
            .is_err()
    );
    // An installed plugin's execute request is protocol 2.
    let e = env();
    e.enabled().await;
    let installed = e.core.plugin(TYPE).unwrap();
    assert_eq!(installed.api_version, 2);
    assert_eq!(
        installed.execute_request(&json!({}), "r", "k")["api_version"],
        2
    );
    let out = crate::plugin::invoke(
        &installed.config,
        installed.execute_request(&json!({"text":"hi"}), "run-1", "key-1"),
    )
    .await
    .unwrap();
    assert_eq!(out["output"]["text"], "demo:hi");
    // PluginError is a distinct, value-free error type.
    let _ = PluginError("x".into());
}

// ---- Real HTTP: authentication, routing, limits and headers --------------------------------
async fn serve_http(core: Arc<Core>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, crate::server::router(core))
            .await
            .unwrap();
    });
    format!("http://{address}")
}

#[tokio::test]
async fn plugin_http_routes_use_the_same_bearer_authentication_as_the_api() {
    let e = env();
    e.enabled().await;
    let base = serve_http(e.core.clone()).await;
    let operator_token = e.core.policy.actors["operator"].token.clone();
    let client = reqwest::Client::new();
    let url = |path: &str| format!("{base}/api/plugins/demo-fixture/{path}");
    // No token, a wrong token and a malformed scheme all fail closed before any plugin runs.
    for request in [
        client.get(url("status")),
        client
            .get(url("status"))
            .bearer_auth("not-a-real-token-not-a-real-token"),
        client
            .get(url("status"))
            .header("authorization", format!("Basic {operator_token}")),
    ] {
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 403);
        assert!(response.text().await.unwrap().contains("error"));
    }
    // Valid token: forwarded to the plugin, with the authenticated principal.
    let response = client
        .get(url("status?x=1"))
        .bearer_auth(&operator_token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["content-type"], "application/json");
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        (body["principal"].as_str(), body["query"]["x"].as_str()),
        (Some("operator"), Some("1"))
    );
    // A read-only actor may GET /status but not POST /echo.
    let viewer = "v".repeat(32);
    assert_eq!(
        client
            .get(url("status"))
            .bearer_auth(&viewer)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let denied = client
        .post(url("echo"))
        .bearer_auth(&viewer)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    // Undeclared route / method, and the declared body limit (1024 bytes) over real HTTP.
    assert_eq!(
        client
            .get(url("nope"))
            .bearer_auth(&operator_token)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        client
            .delete(url("status"))
            .bearer_auth(&operator_token)
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
    let big = client
        .post(url("echo"))
        .bearer_auth(&operator_token)
        .body(vec![b' '; 4096])
        .send()
        .await
        .unwrap();
    assert_eq!(big.status(), 413);
    // An unknown plugin id is a 404 for an authenticated caller.
    let unknown = client
        .get(format!("{base}/api/plugins/ghost/status"))
        .bearer_auth(&operator_token)
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    // After disable the same route answers 503 and the plugin process is not started.
    e.core
        .plugin_disable("operator", "demo-fixture")
        .await
        .unwrap();
    let off = client
        .get(url("status"))
        .bearer_auth(&operator_token)
        .send()
        .await
        .unwrap();
    assert_eq!(off.status(), 503);
}

#[tokio::test]
async fn dashboard_assets_are_served_with_correct_types_and_a_restrictive_csp() {
    let e = env();
    e.enabled().await;
    let base = serve_http(e.core.clone()).await;
    let client = reqwest::Client::new();
    let js = client
        .get(format!("{base}/plugins/demo-fixture/assets/ui/page.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(js.status(), 200);
    assert_eq!(
        js.headers()["content-type"],
        "text/javascript; charset=utf-8"
    );
    assert_eq!(js.headers()["x-content-type-options"], "nosniff");
    let csp = js.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        csp.contains("default-src 'none'")
            && csp.contains("script-src 'self'")
            && csp.contains("frame-ancestors 'none'")
    );
    assert!(
        js.text()
            .await
            .unwrap()
            .contains("export async function mount")
    );
    let css = client
        .get(format!("{base}/plugins/demo-fixture/assets/ui/page.css"))
        .send()
        .await
        .unwrap();
    assert_eq!(css.headers()["content-type"], "text/css; charset=utf-8");
    for path in ["bin/demo-plugin", "manifest.json", "ui/none.js"] {
        let response = client
            .get(format!("{base}/plugins/demo-fixture/assets/{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{path}");
    }
    e.core
        .plugin_disable("operator", "demo-fixture")
        .await
        .unwrap();
    let off = client
        .get(format!("{base}/plugins/demo-fixture/assets/ui/page.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(off.status(), 404, "a disabled plugin serves no assets");
}

#[tokio::test]
async fn plugin_management_operations_go_through_the_authenticated_api() {
    let e = env();
    let base = serve_http(e.core.clone()).await;
    let client = reqwest::Client::new();
    let call = |token: String, method: &'static str, params: Value| {
        let (client, base) = (client.clone(), base.clone());
        async move {
            client
                .post(format!("{base}/api"))
                .bearer_auth(token)
                .json(&json!({"method":method,"params":params}))
                .send()
                .await
                .unwrap()
        }
    };
    let operator = e.core.policy.actors["operator"].token.clone();
    let installed = call(
        operator.clone(),
        "plugin_install",
        json!({"source":e.src()}),
    )
    .await;
    assert_eq!(installed.status(), 200);
    // Lifecycle needs the `plugins` capability.
    let viewer = "v".repeat(32);
    assert_eq!(
        call(
            viewer.clone(),
            "plugin_enable",
            json!({"id":"demo-fixture"})
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        call(
            operator.clone(),
            "plugin_enable",
            json!({"id":"demo-fixture"})
        )
        .await
        .status(),
        200
    );
    // Read-only listing works for the viewer; the snapshot embeds the installed plugins.
    assert_eq!(
        call(viewer.clone(), "plugin_list", json!({}))
            .await
            .status(),
        200
    );
    // (The snapshot embeds this same list as health.installed_plugins; it needs a live
    // DBOS client for schedules, so the live proof checks that.)
    let listed: Value = call(operator.clone(), "plugin_list", json!({}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed["result"][0]["id"], "demo-fixture");
    assert_eq!(
        listed["result"][0]["dashboard"][0]["entry"],
        "/plugins/demo-fixture/assets/ui/page.js"
    );
    assert_eq!(
        e.core.types()[TYPE]["description"],
        "Test fixture plugin: echoes text"
    );
    // Authority-forging parameters are refused like every other operation.
    assert_eq!(
        call(
            operator.clone(),
            "plugin_enable",
            json!({"id":"demo-fixture","actor":"x"})
        )
        .await
        .status(),
        400
    );
    // CLI over the API: declared command, undeclared command, then the disabled denial.
    let run = |command: Vec<&str>| {
        call(
            operator.clone(),
            "plugin_run",
            json!({"id":"demo-fixture","command":command}),
        )
    };
    let hello: Value = run(vec!["hello", "api"]).await.json().await.unwrap();
    assert_eq!(hello["result"]["stdout"], "hello api\n");
    assert_eq!(run(vec!["bogus"]).await.status(), 400);
    assert_eq!(
        call(
            operator.clone(),
            "plugin_disable",
            json!({"id":"demo-fixture"})
        )
        .await
        .status(),
        200
    );
    let denied: Value = run(vec!["hello"]).await.json().await.unwrap();
    assert_eq!(
        (
            denied["result"]["denied"].as_bool(),
            denied["result"]["status"].as_u64()
        ),
        (Some(true), Some(3))
    );
    // MCP tools: listed only while enabled; calls refused after disable.
    let tools: Value = call(operator.clone(), "plugin_tools", json!({}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(tools["result"], json!([]));
    let gone = call(
        operator,
        "plugin_mcp",
        json!({"tool":"plugin_demo-fixture__echo","arguments":{"text":"x"}}),
    )
    .await;
    assert_eq!(gone.status(), 400);
}

// ---- Backup and restore --------------------------------------------------------------
#[tokio::test]
async fn backup_archives_plugin_releases_and_restore_verifies_and_reactivates_them() {
    let e = env();
    e.enabled().await;
    // A crash leftover in staging is debris and must not be archived.
    let staging = Paths::defaults(e.home.path()).plugin_staging();
    std::fs::create_dir_all(staging.join("junk-1")).unwrap();
    let archive_parent = tempfile::tempdir().unwrap();
    let archive = archive_parent.path().join("home");
    let live_paths = Paths::defaults(e.home.path());
    crate::backup::archive_home_for_test(&live_paths, &archive).unwrap();
    assert!(
        !archive.join("plugins/staging").exists(),
        "staging is not archived"
    );
    assert!(
        archive
            .join("plugins/releases/demo-fixture/1.0.0/bin/demo-plugin")
            .is_file()
    );
    // The archived home is a complete, independent home: verify, re-freeze and start it.
    let restored_paths = Paths::defaults(&archive);
    assert_eq!(
        crate::backup::verify_restored_plugins_for_test(&restored_paths).unwrap(),
        1
    );
    let restored = core_at(&archive);
    restored.plugins_startup().await.unwrap();
    assert!(
        restored.plugin(TYPE).is_some(),
        "the enabled plugin is enabled again after restore"
    );
    let hello = restored
        .plugin_run(
            "operator",
            "demo-fixture",
            &["hello".into(), "restored".into()],
        )
        .await
        .unwrap();
    assert_eq!(
        hello["stdout"], "hello restored\n",
        "the restored executable keeps its execute bit"
    );
    let mode = std::fs::metadata(
        restored_paths
            .plugin_release("demo-fixture", "1.0.0")
            .join("ui/page.js"),
    )
    .unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
        0o400,
        "release is read-only again"
    );
    // A tampered archive fails restore verification (digests of the plugin's own manifest).
    let script = restored_paths
        .plugin_release("demo-fixture", "1.0.0")
        .join("bin/demo-plugin");
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    std::fs::write(&script, "#!/bin/sh\necho tampered\n").unwrap();
    let error = crate::backup::verify_restored_plugins_for_test(&restored_paths).unwrap_err();
    assert!(error.to_string().contains("fails verification"), "{error}");
    // And the live home refuses to be backed up once a release no longer matches.
    let live_script = live_paths
        .plugin_release("demo-fixture", "1.0.0")
        .join("bin/demo-plugin");
    std::fs::set_permissions(
        live_script.parent().unwrap(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(
        &live_script,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::write(&live_script, "#!/bin/sh\necho tampered\n").unwrap();
    let blocked =
        crate::backup::archive_home_for_test(&live_paths, &archive_parent.path().join("second"));
    assert!(
        blocked
            .unwrap_err()
            .to_string()
            .contains("fails verification")
    );
}

#[test]
fn backup_refuses_a_plugins_directory_outside_the_home_instead_of_skipping_it() {
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let inside = Paths::defaults(home.path());
    assert!(crate::backup::layout_check_for_test(&inside).is_ok());
    let config = crate::settings::PathsConfig {
        plugins: Some(outside.path().to_path_buf()),
        ..Default::default()
    };
    let relocated = Paths::resolve(home.path(), &config, &|_| None).unwrap();
    let error = crate::backup::layout_check_for_test(&relocated)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("paths.plugins inside the Forge home"),
        "{error}"
    );
}

/// Change only `manifest.json` of an installed release by appending whitespace: it still
/// parses to the same manifest and every declared resource digest stays valid.
fn edit_release_manifest(release: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(release, std::fs::Permissions::from_mode(0o700)).unwrap();
    let manifest = release.join("manifest.json");
    std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut text = std::fs::read(&manifest).unwrap();
    text.push(b'\n');
    std::fs::write(&manifest, text).unwrap();
}

#[tokio::test]
async fn an_edited_release_manifest_fails_check_enable_startup_and_backup_verification() {
    let e = env();
    e.install().await;
    let health = |shown: &Value| shown["health"].clone();
    let pristine = e
        .core
        .plugin_show("operator", "demo-fixture", true)
        .await
        .unwrap();
    assert_eq!(health(&pristine)["release_verified"], true);
    edit_release_manifest(&e.release());
    let shown = e
        .core
        .plugin_show("operator", "demo-fixture", true)
        .await
        .unwrap();
    assert_eq!(health(&shown)["release_verified"], false);
    assert!(
        health(&shown)["error"]
            .as_str()
            .unwrap()
            .contains("manifest.json differs")
    );
    assert!(
        message(
            e.core
                .plugin_enable("operator", "demo-fixture")
                .await
                .unwrap_err()
        )
        .contains("manifest.json differs")
    );
    // A restart classifies it as broken instead of trusting the edited manifest.
    let after = core_at(e.home.path());
    after.plugins_startup().await.unwrap();
    let view = after.plugins_view();
    assert_eq!(view[0]["state"], "broken");
    assert!(
        view[0]["problem"]
            .as_str()
            .unwrap()
            .contains("manifest.json differs")
    );
    // Backup (and so restore) verification compares against the registry record too.
    let archive = tempfile::tempdir().unwrap();
    let error = crate::backup::archive_home_for_test(
        &Paths::defaults(e.home.path()),
        &archive.path().join("a"),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("fails verification") && error.contains("manifest.json differs"),
        "{error}"
    );
}

#[tokio::test]
async fn a_release_without_a_registry_record_is_not_archived() {
    let e = env();
    e.install().await;
    let paths = Paths::defaults(e.home.path());
    assert_eq!(
        crate::plugin_registry::verify_releases(&paths, false).unwrap(),
        1
    );
    e.core
        .store
        .write(|c| {
            c.execute("DELETE FROM forge_plugins", [])?;
            Ok(())
        })
        .unwrap();
    let error = crate::plugin_registry::verify_releases(&paths, false)
        .unwrap_err()
        .to_string();
    assert!(error.contains("no registry record"), "{error}");
}

#[test]
fn install_time_validation_refuses_a_job_that_enable_would_refuse() {
    // A core-type job with invalid input: refused by `plugins check`/install, not at enable.
    let text = refused(
        |f, _| {
            f.insert(
                "jobs/daily.json".into(),
                serde_json::to_vec(
                    &json!({"work":{"type":"echo","input":{"text":5}},"enabled":false}),
                )
                .unwrap(),
            );
        },
        true,
    );
    assert!(
        text.contains("plugin.demo-fixture.daily") && text.contains("invalid"),
        "{text}"
    );
    // A workflow with a forward dependency fails the closed structural validation.
    let text = refused(
        |f, _| {
            f.insert(
                "flows/pipeline.json".into(),
                serde_json::to_vec(&json!({"work":{"type":"workflow","input":{"steps":[
                    {"id":"a","needs":["b"],"action":{"type":"echo","input":{"text":"x"}}},
                    {"id":"b","action":{"type":"echo","input":{"text":"y"}}}]}},"enabled":false}))
                .unwrap(),
            );
        },
        true,
    );
    assert!(
        text.contains("plugin.demo-fixture.pipeline") && text.contains("invalid"),
        "{text}"
    );
}

#[tokio::test]
async fn install_dry_run_shows_the_work_types_the_jobs_will_run_and_authority_gaps() {
    let e = env();
    let dry = e
        .core
        .plugin_install("operator", &e.src(), true)
        .await
        .unwrap();
    assert_eq!(dry["installed"], false);
    let types: Vec<&str> = dry["work_types"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(types.contains(&TYPE), "{types:?}");
    assert!(
        dry["work"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["work_id"] == "plugin.demo-fixture.daily"
                && w["type"] == TYPE
                && w["kind"] == "job")
    );
    // The operator holds the plugin type, so there is no gap.
    assert_eq!(dry["authority_gaps"], json!([]));
    // An installing actor without the type sees the exact gap and the config hint.
    let mut policy = policy_for(e.home.path());
    policy.actors.insert(
        "installer".into(),
        Actor {
            token: "i".repeat(32),
            capabilities: ["read", "register", "execute", "plugins"]
                .map(String::from)
                .to_vec(),
            types: vec!["echo".into()],
        },
    );
    let config = crate::settings::Settings::for_tests(&policy)
        .runtime_config(&policy)
        .unwrap();
    let store = Arc::new(
        AdmissionStore::initialize(&Paths::defaults(e.home.path()), &policy.home_id).unwrap(),
    );
    let core = Core::new(
        Paths::defaults(e.home.path()),
        policy,
        config,
        store,
        BTreeMap::new(),
    )
    .unwrap();
    let dry = core
        .plugin_install("installer", &e.src(), true)
        .await
        .unwrap();
    assert_eq!(dry["authority_gaps"], json!([TYPE, "workflow"]));
    assert!(dry["hints"][0].as_str().unwrap().contains(TYPE));
    // Install succeeds (disabled) but enable names the missing type.
    core.plugin_install("installer", &e.src(), false)
        .await
        .unwrap();
    let error = core
        .plugin_enable("installer", "demo-fixture")
        .await
        .unwrap_err();
    let text = message(error);
    assert!(text.contains(TYPE) && text.contains("`types`"), "{text}");
}

#[tokio::test]
async fn denied_lifecycle_attempts_are_audited_with_actor_and_operation() {
    let e = env();
    e.install().await;
    assert!(
        e.core
            .plugin_install("viewer", &e.src(), false)
            .await
            .is_err()
    );
    assert!(
        e.core
            .plugin_enable("viewer", "demo-fixture")
            .await
            .is_err()
    );
    assert!(
        e.core
            .plugin_disable("stranger", "demo-fixture")
            .await
            .is_err()
    );
    assert!(
        e.core
            .plugin_uninstall("viewer", "demo-fixture")
            .await
            .is_err()
    );
    let rows: Vec<(String, String, String)> = e
        .core
        .store
        .write(|c| {
            let mut q = c.prepare("SELECT actor,target,detail FROM forge_audit WHERE action='plugin_denied' ORDER BY id")?;
            Ok(q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap();
    let ops: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
    assert_eq!(ops, ["install", "enable", "disable", "uninstall"]);
    assert_eq!(rows[0].0, "viewer");
    assert_eq!(rows[2].0, "stranger");
    assert!(rows[1].1 == "demo-fixture");
}

#[tokio::test]
async fn an_unauthorized_actor_gets_403_before_any_disabled_route_state() {
    let e = env();
    e.install().await; // installed but disabled
    // `stranger` lacks the plugin type: denied, not the 503 "installed but disabled".
    let api = e
        .core
        .plugin_api("stranger", "demo-fixture", "GET", "status", "", b"")
        .await;
    assert!(api.err().is_some_and(|x| x.is::<crate::core::Denied>()));
    let run = e
        .core
        .plugin_run("stranger", "demo-fixture", &["hello".to_owned()])
        .await;
    assert!(run.err().is_some_and(|x| x.is::<crate::core::Denied>()));
    // An authorized actor still sees the declared disabled state.
    let reply = e
        .core
        .plugin_api("operator", "demo-fixture", "GET", "status", "", b"")
        .await
        .unwrap();
    assert_eq!(reply.status, 503);
}

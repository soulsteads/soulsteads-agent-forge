//! Tests of the M4 parity operations that need no native engine: schedule-edit storage,
//! tags, doctor (authority gaps, broken config), runner test, definition export/import and
//! the generated service unit. The native-engine behavior (in-place edit firing, doctor's
//! database checks) is proven live by scripts/qualify_mvp.py.
use crate::{
    core::{self, Core, ScheduleContext},
    dbos::admission::AdmissionStore,
    doctor,
    schedule_edit::normalize_tags,
    settings::{Paths, Settings},
};
use rusqlite::params;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

fn core_with(edit: impl FnOnce(&mut core::Policy)) -> (tempfile::TempDir, Arc<Core>) {
    let home = tempfile::tempdir().unwrap();
    core::init(home.path()).unwrap();
    let mut policy = core::load_policy(home.path()).unwrap();
    edit(&mut policy);
    let config = Settings::for_tests(&policy)
        .runtime_config(&policy)
        .unwrap();
    let store = Arc::new(
        AdmissionStore::initialize(&Paths::defaults(home.path()), &policy.home_id).unwrap(),
    );
    let core = Core::new(
        Paths::defaults(home.path()),
        policy,
        config,
        store,
        BTreeMap::new(),
    )
    .unwrap();
    (home, core)
}

#[test]
fn tags_are_normalized_validated_and_bounded() {
    let t = |v: &[&str]| normalize_tags(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(t(&["Beta", "alpha", "beta"]).unwrap(), ["alpha", "beta"]);
    assert!(t(&[""]).is_err());
    assert!(t(&["-lead"]).is_err());
    assert!(t(&["has space"]).is_err());
    assert!(t(&["x".repeat(41).as_str()]).is_err());
    let many: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
    assert!(normalize_tags(&many).is_err());
}

#[test]
fn a_legacy_schedule_table_is_migrated_once_and_keeps_its_rows() {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE forge_schedules(name TEXT PRIMARY KEY,context TEXT NOT NULL,cron TEXT NOT NULL,tz TEXT NOT NULL); INSERT INTO forge_schedules VALUES('n','{}','0 0 * * * *','UTC');").unwrap();
    crate::schedule_edit::migrate(&db).unwrap();
    crate::schedule_edit::migrate(&db).unwrap();
    let (tags, rev, pending): (String, i64, i64) = db
        .query_row(
            "SELECT tags,rev,pending FROM forge_schedules WHERE name='n'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((tags.as_str(), rev, pending), ("[]", 0, 0));
    db.execute(
        "INSERT INTO forge_schedule_revisions VALUES('n',1,'{}','c','z','operator',1.0)",
        [],
    )
    .unwrap();
}

#[test]
fn queued_occurrences_keep_the_revision_they_were_admitted_under() {
    let (_h, core) = core_with(|_| {});
    core.register("operator", "w", json!({"type":"echo","input":{"text":"x"}}))
        .unwrap();
    let ctx = |params: Option<Value>| ScheduleContext {
        home_id: core.policy.home_id.clone(),
        actor: "operator".into(),
        work_id: "w".into(),
        name: "operator.s".into(),
        params,
    };
    let (old, new) = (
        serde_json::to_string(&ctx(None)).unwrap(),
        serde_json::to_string(&ctx(Some(json!({"a":1})))).unwrap(),
    );
    core.store.write(|c| {
        c.execute("INSERT INTO forge_schedules(name,context,cron,tz) VALUES('operator.s',?1,'0 0 * * * *','UTC')", [&old])?;
        Ok(())
    }).unwrap();
    assert!(core.validate_scheduled_context(&ctx(None)).is_ok());
    assert!(
        core.validate_scheduled_context(&ctx(Some(json!({"a":1}))))
            .is_err(),
        "an unrecorded context is refused"
    );
    // An edit: the row now holds the new context, the old one is a retained revision, pending until applied natively.
    core.store.write(|c| {
        c.execute("INSERT INTO forge_schedule_revisions VALUES('operator.s',1,?1,'0 0 * * * *','UTC','operator',1.0)", [&old])?;
        c.execute("UPDATE forge_schedules SET context=?1,cron='0 30 * * * *',rev=1,pending=1 WHERE name='operator.s'", params![new])?;
        Ok(())
    }).unwrap();
    assert!(
        core.validate_scheduled_context(&ctx(None)).is_ok(),
        "an occurrence queued before the edit still validates"
    );
    assert!(
        core.validate_scheduled_context(&ctx(Some(json!({"a":1}))))
            .is_ok()
    );
    let expect = core.schedule_expectations("operator.s").unwrap();
    assert_eq!(
        expect.len(),
        2,
        "while pending, the native schedule may still equal the previous revision"
    );
    assert_eq!(expect[0].1, "0 30 * * * *");
    assert_eq!(expect[1].1, "0 0 * * * *");
    core.store
        .write(|c| {
            c.execute("UPDATE forge_schedules SET pending=0", [])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(core.schedule_expectations("operator.s").unwrap().len(), 1);
    assert_eq!(core.schedule_revision("operator.s").unwrap(), 1);
    // Revisions of another schedule never authorize this one.
    let mut other = ctx(None);
    other.name = "operator.other".into();
    assert!(core.validate_scheduled_context(&other).is_err());
}

// Explicitly opt in to an isolated loopback fixture; never use the normal
// service URL or a database selected from shared environment defaults.
fn packet_pg_url(name: &str, database: &str) -> String {
    let url =
        std::env::var(name).expect("set up the isolated schedule unit-test PostgreSQL database");
    let parsed = reqwest::Url::parse(&url).unwrap();
    assert_eq!(parsed.scheme(), "postgresql");
    assert_eq!(parsed.host_str(), Some("127.0.0.1"));
    assert_eq!(parsed.username(), "forge_schedule_test");
    assert_eq!(parsed.path().trim_start_matches('/'), database);
    assert!(parsed.port().is_some_and(|p| p >= 1024));
    assert!(parsed.query().is_none() && parsed.password().is_none());
    url
}

async fn packet_pg_core(home: &std::path::Path, url: &str) -> (Arc<Core>, crate::dbos::Runtime) {
    let policy = core::load_policy(home).unwrap();
    let settings = Settings::resolve(
        home.to_owned(),
        &policy,
        &crate::settings::Overrides {
            database_url: Some(url.to_owned()),
            ..Default::default()
        },
        &|_| None,
    )
    .unwrap();
    let paths = settings.paths.clone();
    let config = settings.runtime_config(&policy).unwrap();
    let store = Arc::new(AdmissionStore::initialize(&paths, &policy.home_id).unwrap());
    let core = Core::new(
        paths.clone(),
        policy,
        config.clone(),
        store.clone(),
        BTreeMap::new(),
    )
    .unwrap();
    core.settings.set(settings).unwrap();
    let mut runtime = crate::dbos::Runtime::new(&paths, config.clone()).unwrap();
    let owner = core.clone();
    runtime
        .register_workflow(crate::core::SCHEDULED, move |input| {
            crate::workflows::scheduled(owner.clone(), input)
        })
        .unwrap();
    runtime.application = Some(core.clone());
    assert!(
        core.bridge
            .set(runtime.install_submission_bridge(store).unwrap())
            .is_ok()
    );
    runtime.prepare().await.unwrap();
    core.client
        .set(config.connect_client().await.unwrap())
        .ok()
        .unwrap();
    (core, runtime)
}

fn packet_sql(url: &str, sql: &str) {
    let result = std::process::Command::new("psql")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .args([
            "--no-psqlrc",
            "--no-password",
            url,
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            sql,
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[tokio::test]
#[ignore = "requires the isolated schedule unit-test PostgreSQL database"]
async fn native_schedule_error_keeps_pending_revision_for_restart() {
    use crate::schedule_edit::ScheduleEdit;
    let url = packet_pg_url("FORGE_SCHEDULE_TEST_POSTGRES", "forge_schedule_candidate");
    let home = tempfile::tempdir().unwrap();
    core::init(home.path()).unwrap();
    let (core, runtime) = packet_pg_core(home.path(), &url).await;
    core.register(
        "operator",
        "w",
        json!({"type":"echo","input":{"text":"fixture"}}),
    )
    .unwrap();
    core.schedule("operator", "s", "w", "0 0 * * * *", "UTC")
        .await
        .unwrap();
    let name = "operator.s";
    core.schedule("operator", "z-recover", "w", "0 0 * * * *", "UTC")
        .await
        .unwrap();
    let sibling = "operator.z-recover";
    let schema = &core.config.schema;
    assert!(
        schema
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    );
    let table = format!("{schema}.workflow_schedules");
    // Reject the same timing edit in both names. Remove only the sibling's
    // rejection before reopen, leaving the first name persistently pending.
    packet_sql(
        &url,
        &format!(
            "ALTER TABLE {table} ADD CONSTRAINT reject_fixture_edit CHECK
            (schedule_name <> 'operator.s' OR schedule <> '0 17 * * * *'),
         ADD CONSTRAINT reject_fixture_sibling CHECK
            (schedule_name <> 'operator.z-recover' OR schedule <> '0 17 * * * *')"
        ),
    );
    for target in [name, sibling] {
        assert!(
            core.schedule_update(
                "operator",
                target,
                ScheduleEdit {
                    cron: Some("0 17 * * * *".into()),
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
    }
    packet_sql(
        &url,
        &format!("ALTER TABLE {table} DROP CONSTRAINT reject_fixture_sibling"),
    );
    let pending = core
        .store
        .write(|c| {
            Ok(c.query_row(
                "SELECT rev,pending FROM forge_schedules WHERE name=?1",
                [name],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, bool>(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(
        pending,
        (1, true),
        "retain recoverable edit after a native update error"
    );
    let expected = core.schedule_expectations(name).unwrap();
    assert_eq!(expected.len(), 2);
    assert_eq!(expected[0].1, "0 17 * * * *");
    assert_eq!(expected[1].1, "0 0 * * * *");
    let blocked = core
        .schedule_update(
            "operator",
            name,
            ScheduleEdit {
                tz: Some("America/Chicago".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(blocked.to_string().contains("pending edit"));
    let trigger = core.schedule_trigger("operator", name).await.unwrap_err();
    assert!(trigger.to_string().contains("pending edit"));
    assert_eq!(
        core.native()
            .unwrap()
            .get_schedule(name)
            .await
            .unwrap()
            .unwrap()
            .schedule(),
        "0 0 * * * *"
    );
    core.native().unwrap().close().await;
    drop(runtime);
    drop(core);
    let (reopened, runtime) = packet_pg_core(home.path(), &url).await;
    let heal = reopened.heal_schedule_edits().await;
    let active = reopened
        .schedule_show("operator", name, Some(5))
        .await
        .unwrap();
    assert_eq!(active["status"], "ACTIVE");
    assert_eq!(active["schedule"], "0 0 * * * *");
    assert_eq!(active["pending_edit"]["schedule"], "0 17 * * * *");
    assert_eq!(active["pending_edit"]["revision"], 1);
    assert_eq!(active["actions"]["pause"], true);
    assert_eq!(active["actions"]["delete"], false);
    assert!(active["upcoming"].as_array().unwrap().iter().all(|tick| {
        chrono::DateTime::parse_from_rfc3339(tick["utc"].as_str().unwrap())
            .unwrap()
            .format("%M")
            .to_string()
            == "00"
    }));
    let definitions_before_pause = reopened.schedule_expectations(name).unwrap();
    let revisions_before_pause = reopened.schedule_revision_contexts(name).unwrap();
    let pause = reopened.schedule_action("operator", name, "pause").await;
    let sibling_row = reopened
        .native()
        .unwrap()
        .get_schedule(sibling)
        .await
        .unwrap()
        .unwrap();
    let primary = reopened
        .native()
        .unwrap()
        .get_schedule(name)
        .await
        .unwrap()
        .unwrap();
    let evidence = (
        heal.is_ok(),
        pause.is_ok(),
        sibling_row.schedule(),
        primary.status().as_str(),
    );
    assert_eq!(
        evidence,
        (true, true, "0 17 * * * *", "PAUSED"),
        "persistent rejection must isolate healing and preserve authorized pause"
    );
    assert_eq!(heal.unwrap(), [name]);
    // The server prepares again before launch. The unresolved but authorized
    // old revision must still pass the existing native policy validation.
    runtime.prepare().await.unwrap();
    assert_eq!(definitions_before_pause.len(), 2);
    assert_eq!(
        reopened.schedule_expectations(name).unwrap(),
        definitions_before_pause
    );
    assert_eq!(
        reopened.schedule_revision_contexts(name).unwrap(),
        revisions_before_pause
    );
    assert_eq!(reopened.schedule_expectations(sibling).unwrap().len(), 1);
    assert_eq!(primary.schedule(), "0 0 * * * *");
    for action in ["resume", "delete"] {
        assert!(
            reopened
                .schedule_action("operator", name, action)
                .await
                .unwrap_err()
                .to_string()
                .contains("pending edit")
        );
    }
    packet_sql(
        &url,
        &format!("ALTER TABLE {table} DROP CONSTRAINT reject_fixture_edit"),
    );
    let healed = ScheduleEdit {
        cron: Some("0 17 * * * *".into()),
        ..Default::default()
    };
    reopened
        .schedule_update("operator", name, healed)
        .await
        .unwrap();
    let schedule = reopened
        .native()
        .unwrap()
        .get_schedule(name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(schedule.schedule(), "0 17 * * * *");
    assert_eq!(schedule.status().as_str(), "PAUSED");
    assert!(schedule.last_fired_at().is_none());
    let settled = reopened
        .schedule_show("operator", name, Some(5))
        .await
        .unwrap();
    assert_eq!(settled["status"], "PAUSED");
    assert_eq!(settled["schedule"], "0 17 * * * *");
    assert!(settled["pending_edit"].is_null());
    assert_eq!(settled["actions"]["resume"], true);

    assert_eq!(
        reopened.schedule_revision_contexts(name).unwrap(),
        revisions_before_pause
    );
    reopened.validate_schedule(&schedule).unwrap();
    assert!(reopened.heal_schedule_edits().await.unwrap().is_empty());
    assert_eq!(reopened.schedule_revision(name).unwrap(), 1);
    let lock = reopened.lock_schedule(name).await.unwrap();
    let mut waiting = Box::pin(reopened.schedule_update(
        "operator",
        name,
        ScheduleEdit {
            cron: Some("0 37 * * * *".into()),
            ..Default::default()
        },
    ));
    assert!(futures::poll!(&mut waiting).is_pending());
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reopened.schedule("operator", "other", "w", "0 0 * * * *", "UTC"),
    )
    .await
    .expect("another name must progress")
    .unwrap();
    assert_eq!(reopened.schedule_revision(name).unwrap(), 1);
    drop(lock);
    let second = reopened.schedule_update(
        "operator",
        name,
        ScheduleEdit {
            tz: Some("America/Chicago".into()),
            ..Default::default()
        },
    );
    let (first, second) = tokio::join!(waiting, second);
    first.unwrap();
    second.unwrap();
    let final_row = reopened
        .native()
        .unwrap()
        .get_schedule(name)
        .await
        .unwrap()
        .unwrap();
    reopened.validate_schedule(&final_row).unwrap();
    assert_eq!(final_row.schedule(), "0 37 * * * *");
    assert_eq!(final_row.cron_timezone(), Some("America/Chicago"));
    assert_eq!(reopened.schedule_revision(name).unwrap(), 3);
    let unchanged = reopened
        .schedule_update(
            "operator",
            name,
            ScheduleEdit {
                cron: Some("0 37 * * * *".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(unchanged["changed"], json!([]));
    assert_eq!(reopened.schedule_revision(name).unwrap(), 3);
    assert_eq!(reopened.schedule_expectations(name).unwrap().len(), 1);
    let workflows = reopened
        .native()
        .unwrap()
        .list_workflows(&Default::default())
        .await
        .unwrap();
    assert!(
        workflows.is_empty(),
        "prepare and healing do not trigger workflows"
    );
    let admissions = reopened
        .store
        .write(|c| {
            Ok(
                c.query_row("SELECT count(*) FROM dbos_invocations", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            )
        })
        .unwrap();
    assert_eq!(admissions, 0, "no Forge work was admitted");
    reopened.native().unwrap().close().await;
}

#[test]
fn doctor_names_the_exact_config_change_for_a_pre_m3_home() {
    let (_h, core) = core_with(|p| {
        let a = p.actors.get_mut("operator").unwrap();
        a.capabilities
            .retain(|c| !["approve", "configure", "transfer"].contains(&c.as_str()));
        a.types.retain(|t| t != "approval");
        p.actors.insert(
            "reader".into(),
            core::Actor {
                token: "r".repeat(32),
                capabilities: vec!["read".into()],
                types: vec![],
            },
        );
    });
    let checks = doctor::authority_checks(&core.policy);
    assert_eq!(
        checks.len(),
        1,
        "a deliberately narrow reader actor is not flagged: {checks:?}"
    );
    let c = &checks[0];
    assert_eq!(
        (c["id"].as_str(), c["status"].as_str()),
        (Some("authority.operator"), Some("warn"))
    );
    let fix = c["fix"].as_str().unwrap();
    for word in [
        "approve",
        "configure",
        "transfer",
        "approval",
        "\"actors\"",
        "\"operator\"",
    ] {
        assert!(fix.contains(word), "{word} missing from {fix}");
    }
    assert!(
        !fix.contains(&core.policy.actors["operator"].token),
        "the snippet never prints a token"
    );
    // The error the operator actually sees points at doctor, and doctor lists that capability.
    let err = core
        .permit("operator", "approve", None)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("soulsteads-forge doctor") && err.contains("approve"),
        "{err}"
    );
    let (_h2, fresh) = core_with(|_| {});
    assert_eq!(doctor::authority_checks(&fresh.policy)[0]["status"], "ok");
}

#[tokio::test]
async fn offline_doctor_fails_a_broken_config_and_passes_a_clean_one() {
    let home = tempfile::tempdir().unwrap();
    core::init(home.path()).unwrap();
    let env = |_: &str| None;
    let clean = doctor::offline(home.path().to_path_buf(), &Default::default(), &env).await;
    let status = |r: &Value, id: &str| {
        r["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .map(|c| c["status"].clone())
    };
    assert_eq!(status(&clean, "config"), Some(json!("ok")), "{clean}");
    assert_eq!(status(&clean, "authority"), Some(json!("ok")));
    assert_eq!(status(&clean, "definitions"), Some(json!("skip")));
    // Without a database the report fails, naming the missing setting and how to fix it.
    assert_eq!(clean["status"], "fail");
    assert_eq!(status(&clean, "database"), Some(json!("fail")));
    let file = crate::settings::config_file(home.path());
    let mut cfg: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    cfg["paths"] = json!({"statee": "/x"});
    std::fs::write(&file, cfg.to_string()).unwrap();
    let broken = doctor::offline(home.path().to_path_buf(), &Default::default(), &env).await;
    assert_eq!(broken["status"], "fail");
    let first = &broken["checks"][0];
    assert_eq!(first["id"], "config");
    assert!(
        first["detail"]
            .as_str()
            .unwrap()
            .contains("unknown field `statee`"),
        "{first}"
    );
    assert!(doctor::render(&broken).contains("FAIL"));
    assert!(
        !doctor::render(&broken).contains(
            &cfg["actors"]["operator"]["token"]
                .as_str()
                .unwrap()
                .to_owned()
        )
    );
}

#[tokio::test]
async fn runner_test_probes_one_profile_without_a_model() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("harness");
    std::fs::write(&exe, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let profile = |path: &std::path::Path| {
        serde_json::from_value(
            json!({"adapter":"stdio","executable":path,"permission_mode":"read-only"}),
        )
        .unwrap()
    };
    let (_h, core) = core_with(|p| {
        p.runners.insert("ok".into(), profile(&exe));
        p.runners
            .insert("gone".into(), profile(&dir.path().join("missing")));
    });
    let good = core.runner_test("operator", "ok").await.unwrap();
    assert_eq!(good["available"], true, "{good}");
    assert!(
        good["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["ok"] == true)
    );
    assert!(good["note"].as_str().unwrap().contains("no model"));
    let bad = core.runner_test("operator", "gone").await.unwrap();
    assert_eq!(bad["available"], false);
    assert_eq!(bad["checks"][0]["id"], "executable");
    assert!(core.runner_test("operator", "nope").await.is_err());
    assert!(core.runner_test("stranger", "ok").await.is_err());
}

fn work(id: &str, text: &str, enabled: bool) -> Value {
    json!({"id":id,"definition":{"type":"echo","input":{"text":text}},"enabled":enabled})
}

#[tokio::test]
async fn import_is_validated_first_idempotent_and_refuses_collisions_unless_replace() {
    let (_h, core) = core_with(|_| {});
    let doc = json!({"format":"forge-definitions/v1","work":[work("a","one",true), work("b","two",false)]});
    let plan = core
        .definitions_import("operator", &doc, false, true)
        .await
        .unwrap();
    assert_eq!(
        (plan["applied"].clone(), plan["dry_run"].clone()),
        (json!(false), json!(true))
    );
    assert!(
        core.work("operator", "a").is_err(),
        "a dry run writes nothing"
    );
    let done = core
        .definitions_import("operator", &doc, false, false)
        .await
        .unwrap();
    assert_eq!(done["applied"], true, "{done}");
    assert_eq!(core.work("operator", "b").unwrap()["enabled"], false);
    let again = core
        .definitions_import("operator", &doc, false, false)
        .await
        .unwrap();
    assert!(
        again["work"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["action"] == "unchanged"),
        "{again}"
    );
    // Export round-trips into a second home.
    let exported = core.definitions_export("operator").await.unwrap();
    assert_eq!(exported["work"].as_array().unwrap().len(), 2);
    let (_h2, other) = core_with(|_| {});
    assert_eq!(
        other
            .definitions_import("operator", &exported, false, false)
            .await
            .unwrap()["applied"],
        true
    );
    assert_eq!(
        other.definitions_export("operator").await.unwrap()["work"],
        exported["work"]
    );
    // A collision refuses the WHOLE document, including its valid items.
    let changed = json!({"format":"forge-definitions/v1","work":[work("a","DIFFERENT",true), work("c","new",true)]});
    let refused = core
        .definitions_import("operator", &changed, false, false)
        .await
        .unwrap();
    assert_eq!(refused["applied"], false);
    assert!(
        refused["issues"][0].as_str().unwrap().contains("collision"),
        "{refused}"
    );
    assert!(
        core.work("operator", "c").is_err(),
        "nothing from a refused import is written"
    );
    let replaced = core
        .definitions_import("operator", &changed, true, false)
        .await
        .unwrap();
    assert_eq!(replaced["applied"], true, "{replaced}");
    assert_eq!(
        core.work("operator", "a").unwrap()["definition"]["input"]["text"],
        "DIFFERENT"
    );
}

#[tokio::test]
async fn import_applies_authority_and_rejects_malformed_documents() {
    let (_h, core) = core_with(|p| {
        p.actors.insert(
            "narrow".into(),
            core::Actor {
                token: "n".repeat(32),
                capabilities: vec!["read".into(), "register".into()],
                types: vec!["echo".into()],
            },
        );
    });
    // Over-authority: the narrow actor may not register a harness/fail type.
    let over = json!({"format":"forge-definitions/v1","work":[work("ok","x",true), {"id":"deny","definition":{"type":"fail","input":{}},"enabled":true}]});
    let r = core
        .definitions_import("narrow", &over, false, false)
        .await
        .unwrap();
    assert_eq!(r["applied"], false);
    assert!(
        r["issues"].to_string().contains("deny") && r["issues"].to_string().contains("authority"),
        "{r}"
    );
    assert!(core.work("narrow", "ok").is_err());
    for bad in [
        json!({"format":"other"}),
        json!({"format":"forge-definitions/v1","work":"x"}),
        json!({"format":"forge-definitions/v1","extra":1}),
        json!({"format":"forge-definitions/v1","work":[{"id":"x","definition":{},"enabled":true,"owner":"y"}]}),
    ] {
        assert!(
            core.definitions_import("narrow", &bad, false, false)
                .await
                .is_err(),
            "{bad}"
        );
    }
    // Plugin-reserved ids and ids owned by another actor are refused, not overwritten.
    let reserved = json!({"format":"forge-definitions/v1","work":[work("plugin.x","x",true)]});
    assert_eq!(
        core.definitions_import("narrow", &reserved, false, false)
            .await
            .unwrap()["applied"],
        false
    );
    core.register(
        "operator",
        "mine",
        json!({"type":"echo","input":{"text":"x"}}),
    )
    .unwrap();
    let steal = json!({"format":"forge-definitions/v1","work":[work("mine","x",true)]});
    let r = core
        .definitions_import("narrow", &steal, true, false)
        .await
        .unwrap();
    assert!(r["issues"].to_string().contains("another actor"), "{r}");
    // Without the register capability nothing is read, let alone written.
    assert!(
        core.definitions_import("stranger-without-actor", &steal, false, false)
            .await
            .is_err()
    );
}

#[test]
fn the_service_unit_never_carries_the_database_credential() {
    use std::os::unix::fs::PermissionsExt;
    let (home, core) = core_with(|_| {});
    let settings = Settings::for_tests(&core.policy);
    let exe = std::path::Path::new("/opt/forge bin/soulsteads-forge");
    let refused = crate::service_unit::unit_text(&settings, &core.policy, exe, None)
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("--env-file") && refused.contains("FORGE_DATABASE_URL"),
        "{refused}"
    );
    let env = home.path().join("service.env");
    std::fs::write(
        &env,
        format!(
            "FORGE_DATABASE_URL={}://{}:{}@127.0.0.1:5432/d\n",
            "postgresql", "u", "SECRETPW"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        crate::service_unit::unit_text(&settings, &core.policy, exe, Some(&env))
            .unwrap_err()
            .to_string()
            .contains("0600")
    );
    std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o600)).unwrap();
    let text = crate::service_unit::unit_text(&settings, &core.policy, exe, Some(&env)).unwrap();
    assert!(text.starts_with("# Managed-By: agent-forge\n"));
    assert!(text.contains(&format!("EnvironmentFile={}", env.display())));
    assert!(text.contains("ExecStart=\"/opt/forge bin/soulsteads-forge\" --home "));
    assert!(text.contains(&format!(
        "--host {} --port {}",
        settings.server.host, settings.server.port
    )));
    assert!(
        !text.contains("SECRETPW") && !text.contains("postgresql://"),
        "{text}"
    );
    // An env file that does not define the variable is refused; names only are read.
    std::fs::write(&env, "OTHER=1\n").unwrap();
    assert!(
        crate::service_unit::unit_text(&settings, &core.policy, exe, Some(&env))
            .unwrap_err()
            .to_string()
            .contains("does not define")
    );
    // database.url_env: that name is the one required.
    let (_h2, named) = core_with(|p| p.database.url_env = Some("MY_DB_URL".into()));
    let s2 = Settings::resolve(
        home.path().to_path_buf(),
        &named.policy,
        &Default::default(),
        &|_| None,
    )
    .unwrap();
    assert!(
        crate::service_unit::unit_text(&s2, &named.policy, exe, None)
            .unwrap_err()
            .to_string()
            .contains("MY_DB_URL")
    );
    // Names are validated.
    assert_eq!(
        crate::service_unit::unit_name(Some("forge-m4-test-ab12")).unwrap(),
        "forge-m4-test-ab12.service"
    );
    assert_eq!(
        crate::service_unit::unit_name(None).unwrap(),
        "agent-forge.service"
    );
    for bad in ["", "Up", "a b", "../x", "-x", "x.service"] {
        assert!(crate::service_unit::unit_name(Some(bad)).is_err(), "{bad}");
    }
}

#[test]
fn system_service_preview_runs_as_a_non_root_account_without_loading_app_settings() {
    let text = crate::service_unit::system_unit_text(
        std::path::Path::new("/srv/forge/home"),
        std::path::Path::new("/usr/local/bin/soulsteads-forge"),
        "forge-service",
        None,
    )
    .unwrap();
    assert!(text.contains("User=forge-service\n"), "{text}");
    assert!(
        text.contains(
            "ExecStart=\"/usr/local/bin/soulsteads-forge\" --home \"/srv/forge/home\" serve\n"
        ),
        "{text}"
    );
    assert!(!text.contains("--host"), "{text}");
    assert!(!text.contains("postgresql://"), "{text}");
    assert!(
        crate::service_unit::system_unit_text(
            std::path::Path::new("/srv/forge/home"),
            std::path::Path::new("/usr/local/bin/soulsteads-forge"),
            "root",
            None,
        )
        .is_err()
    );
}

#[test]
fn system_preview_uses_the_root_protected_system_unit_directory_without_systemd() {
    let result = crate::service_unit::run_system(
        "preview",
        std::path::Path::new("/srv/forge/home"),
        std::path::Path::new("/usr/local/bin/soulsteads-forge"),
        crate::service_unit::SystemRequest {
            name: None,
            env_file: None,
            run_as: "forge-service",
            start: false,
        },
    )
    .unwrap();
    assert_eq!(result["unit"], "agent-forge.service");
    assert_eq!(result["path"], "/etc/systemd/system/agent-forge.service");
    assert!(
        result["text"]
            .as_str()
            .unwrap()
            .contains("User=forge-service\n")
    );
}

#[tokio::test]
async fn uninstall_touches_only_units_this_tool_generated_for_this_home() {
    let (home, core) = core_with(|_| {});
    let settings = Settings::for_tests(&core.policy);
    let dir = home.path().join("units");
    std::fs::create_dir_all(&dir).unwrap();
    let foreign = dir.join("someone-elses.service");
    std::fs::write(&foreign, "[Service]\nExecStart=/bin/true\n").unwrap();
    let req = |name| crate::service_unit::Request {
        name: Some(name),
        env_file: None,
        start: false,
    };
    let err = crate::service_unit::run(
        "uninstall",
        &settings,
        &core.policy,
        req("someone-elses"),
        dir.clone(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("untouched"), "{err}");
    assert!(foreign.exists());
    let other_home = dir.join("other.service");
    std::fs::write(
        &other_home,
        format!(
            "{}\n# Home: /another/home\n[Service]\n",
            crate::settings::SERVICE_MARKER
        ),
    )
    .unwrap();
    assert!(
        crate::service_unit::run(
            "uninstall",
            &settings,
            &core.policy,
            req("other"),
            dir.clone()
        )
        .await
        .is_err()
    );
    assert!(other_home.exists());
    let missing = crate::service_unit::run(
        "uninstall",
        &settings,
        &core.policy,
        req("absent"),
        dir.clone(),
    )
    .await
    .unwrap();
    assert_eq!(missing["removed"], false);
    let status = crate::service_unit::run(
        "status",
        &settings,
        &core.policy,
        req("someone-elses"),
        dir.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        (
            status["installed"].clone(),
            status["managed_by_agent_forge"].clone()
        ),
        (json!(true), json!(false))
    );
}

#[test]
fn an_unknown_work_type_is_invalid_input_not_an_authority_gap() {
    let (_h, core) = core_with(|_| {});
    let error = core
        .check_definition("operator", &json!({"type":"nope","input":{}}))
        .unwrap_err();
    assert!(error.downcast_ref::<core::Invalid>().is_some(), "{error:#}");
    assert!(
        !error.to_string().contains("doctor"),
        "never advise granting a type that does not exist"
    );
    let known_but_denied = core_with(|p| {
        p.actors.get_mut("operator").unwrap().types = vec!["echo".into()];
    })
    .1
    .check_definition("operator", &json!({"type":"fail","input":{}}))
    .unwrap_err();
    assert!(
        known_but_denied.to_string().contains("work type `fail`")
            && known_but_denied.to_string().contains("doctor")
    );
}

/// Regression for the online-backup stall: the backup holds the admission store for the
/// whole dump, and requests waiting for it must not occupy every runtime worker (the old
/// behavior stalled all requests and the dump itself behind them).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_for_the_admission_store_does_not_starve_the_runtime() {
    let home = tempfile::tempdir().unwrap();
    core::init(home.path()).unwrap();
    let policy = core::load_policy(home.path()).unwrap();
    let store = Arc::new(
        AdmissionStore::initialize(&Paths::defaults(home.path()), &policy.home_id).unwrap(),
    );
    let holder = store.clone();
    let hold = tokio::task::spawn_blocking(move || {
        holder
            .write(|_| {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                Ok(())
            })
            .unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let waiters: Vec<_> = (0..6)
        .map(|_| {
            let s = store.clone();
            tokio::spawn(async move { s.inspect("absent").unwrap() })
        })
        .collect();
    let started = std::time::Instant::now();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        started.elapsed() < std::time::Duration::from_millis(900),
        "timers and other tasks must keep running while six requests wait for the store: {:?}",
        started.elapsed()
    );
    hold.await.unwrap();
    for w in waiters {
        assert!(w.await.unwrap().is_none());
    }
}

#[tokio::test]
async fn online_backup_waits_for_lifecycle_before_preflight() {
    let (_home, core) = core_with(|_| {});
    let settings = Settings::resolve(
        core.paths.home.clone(),
        &core.policy,
        &Default::default(),
        &|_| None,
    )
    .unwrap();
    assert!(settings.database.url().is_err());
    let output = tempfile::tempdir().unwrap();
    let destination = output.path().join("backup");
    let lifecycle = core.registry.lock_lifecycle().await;
    let mut pending = Box::pin(crate::backup::backup_online(
        settings,
        destination.clone(),
        &core,
    ));
    assert!(futures::poll!(&mut pending).is_pending());
    assert!(!destination.exists());
    drop(lifecycle);
    let error = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
        .await
        .expect("backup should finish its preflight")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no PostgreSQL database configured")
    );
    assert!(!destination.exists());
    let _released = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        core.registry.lock_lifecycle(),
    )
    .await
    .expect("backup must release the lifecycle lock when preflight fails");
}

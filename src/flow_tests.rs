//! Native boundary tests; actual DBOS scheduling/model/browser gates run separately.
use crate::{
    core::{self, Core, hash},
    dbos::admission::{AdmissionStore, Envelope, Invocation},
    plugin::{Plugin, PluginConfig},
    safety,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

fn fixture() -> (tempfile::TempDir, Arc<Core>) {
    let home = tempfile::tempdir().unwrap();
    core::init(home.path()).unwrap();
    let mut policy = core::load_policy(home.path()).unwrap();
    policy
        .actors
        .get_mut("operator")
        .unwrap()
        .types
        .push("plugin.record".into());
    policy.models.insert(
        "local".into(),
        core::Model {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "test".into(),
            key_env: None,
        },
    );
    let count = home.path().join("count");
    let code = format!(
        "import json,sys,pathlib\np=pathlib.Path({:?})\nn=int(p.read_text())+1 if p.exists() else 1\np.write_text(str(n))\nr=json.load(sys.stdin)\nif r['input'].get('fail'):sys.exit(1)\nprint(json.dumps(dict(api_version=1,output=dict(count=n,value=r['input']['value']))))",
        count.to_string_lossy()
    );
    let config = PluginConfig {
        command: vec!["/usr/bin/python3".into(), "-c".into(), code],
        timeout_seconds: 20,
        env_passthrough: vec![],
    };
    let plugin = Plugin {
        config,
        description: "Test-only receipt fixture".into(),
        schema: json!({"type":"object","properties":{"value":{},"fail":{"type":"boolean"}},"required":["value"],"additionalProperties":false}),
        api_version: 1,
    };
    let config = crate::settings::Settings::for_tests(&policy)
        .runtime_config(&policy)
        .unwrap();
    let store = Arc::new(
        AdmissionStore::initialize(
            &crate::settings::Paths::defaults(home.path()),
            &policy.home_id,
        )
        .unwrap(),
    );
    let core = Core::new(
        crate::settings::Paths::defaults(home.path()),
        policy,
        config,
        store,
        BTreeMap::from([("plugin.record".into(), plugin)]),
    )
    .unwrap();
    (home, core)
}
#[test]
fn plugin_discovery_and_validation_never_dispatch_or_publish_configuration() {
    let (home, core) = fixture();
    let types = core.types();
    assert_eq!(
        types["plugin.record"]["description"],
        "Test-only receipt fixture"
    );
    assert!(!types.to_string().contains("/usr/bin/python3"));
    assert!(types["plugin.record"].get("command").is_none());
    let valid = json!({"type":"plugin.record","input":{"value":"hello"}});
    assert_eq!(
        core.validate_only("operator", &valid).unwrap()["valid"],
        true
    );
    let invalid = json!({"type":"plugin.record","input":{"fail":"PRIVATE_VALUE"}});
    let diagnostic = core
        .validate_only("operator", &invalid)
        .unwrap_err()
        .to_string();
    assert!(diagnostic.contains("/input/value"));
    assert!(diagnostic.contains("/input/fail"));
    assert!(!diagnostic.contains("PRIVATE_VALUE"));
    assert_eq!(
        core.register("operator", "invalid", invalid)
            .unwrap_err()
            .to_string(),
        diagnostic
    );
    assert!(
        core.validate_only("operator", &json!({"type":"plugin.unknown","input":{}}))
            .is_err()
    );
    assert!(core.validate_only("unknown-actor", &valid).is_err());
    assert!(core.work("operator", "invalid").is_err());
    assert!(!home.path().join("count").exists());
}

fn admitted(core: &Core, key: &str, input: Option<Value>) -> Envelope {
    let definition = json!({"type":"workflow","input":{"steps":[{"id":"one","action":{"type":"plugin.record","input":{"value":"retained"}}}]}});
    core.register("operator", "flow", definition.clone())
        .unwrap();
    let params = input
        .map(|v| json!({"input":core.params_reference("operator",&v).unwrap()}))
        .unwrap_or_else(|| json!({}));
    let invocation=Invocation::from_json(&json!({"format_version":1,"home_id":core.policy.home_id,"scope":"operator","workflow_name":core::WORKFLOW,"request_id":key,"workflow_version":core::VERSION,"principal":"operator","workspace":"flow","plugin":"workflow","resource":"bounded-work-queue","policy_ref":"configured-actor","definition_sha256":hash(crate::canonical::to_vec(&definition).unwrap()),"action_sha256":core.action_hash("operator",&definition).unwrap(),"parameters":params,"evidence":[],"authorization_expires_at_ms":null}).to_string()).unwrap();
    core.store.admit(&invocation).unwrap().envelope
}
#[test]
fn registration_schema_diagnostics_reject_without_admission() {
    let (_home, core) = fixture();
    let valid = json!({"type":"workflow","input":{"steps":[{"id":"one","action":{"type":"echo","input":{"text":"controlled"}}}]}});
    let mut retained_shape = valid.clone();
    retained_shape["input"]["type"] = json!("workflow");
    let cases = [
        (retained_shape, "unexpected property \"/input/type\""),
        (
            json!({"type":"workflow","input":{}}),
            "missing required property \"/input/steps\"",
        ),
        (
            json!({"type":"workflow","input":{"steps":"PRIVATE_VALUE"}}),
            "expected array at \"/input/steps\"",
        ),
        (
            json!({"type":"workflow","input":{"steps":[{"id":17,"action":{"type":"echo","input":{"text":"controlled"}}}]}}),
            "expected string at \"/input/steps/0/id\"",
        ),
    ];
    for (definition, expected) in cases {
        let error = core
            .register("operator", "invalid", definition)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("work input does not match supported schema: {expected}")
        );
        assert!(core.work("operator", "invalid").is_err());
    }
    core.store
        .write(|db| {
            for table in [
                "forge_work",
                "forge_schedules",
                "dbos_invocations",
                "dbos_effect_receipts",
                "forge_flow_effects",
                "forge_audit",
            ] {
                assert_eq!(
                    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                        .get::<_, i64>(0))?,
                    0
                );
            }
            Ok(())
        })
        .unwrap();
    core.register("operator", "control", valid.clone()).unwrap();
    assert_eq!(
        core.work("operator", "control").unwrap()["definition"],
        valid
    );
    let mut changed = valid.clone();
    changed["input"]["steps"][0]["action"]["input"]["text"] = json!("changed");
    assert!(core.register("operator", "control", changed).is_err());
    assert_eq!(
        core.work("operator", "control").unwrap()["definition"],
        valid
    );
}
#[test]
fn immutable_params_are_scoped_integrity_checked_and_conflict_on_same_key() {
    let (_home, core) = fixture();
    let e = admitted(&core, "params", Some(json!({"task":7})));
    assert_eq!(core.invocation_params(&e).unwrap(), json!({"task":7}));
    let mut changed = e.invocation.source_value();
    changed["parameters"]["input"] = core
        .params_reference("operator", &json!({"task":8}))
        .unwrap();
    let inv = Invocation::from_json(&changed.to_string()).unwrap();
    assert_eq!(inv.dbos_id(), e.dbos_id);
    assert!(core.store.admit(&inv).is_err());
    core.store
        .write(|c| {
            c.execute("UPDATE forge_inputs SET payload='{}'", [])?;
            Ok(())
        })
        .unwrap();
    assert!(core.invocation_params(&e).is_err());
}
#[test]
fn literal_invalid_leaves_and_internal_agent_turn_are_refused_before_registration() {
    let (_home, core) = fixture();
    let invalid = json!({"type":"workflow","input":{"steps":[{"id":"good","action":{"type":"plugin.record","input":{"value":1}}},{"id":"bad","action":{"type":"plugin.record","input":{}}}]}});
    assert!(core.register("operator", "invalid", invalid).is_err());
    assert!(core.work("operator", "invalid").is_err());
    let internal = json!({"type":"workflow","input":{"steps":[{"id":"raw","action":{"type":"agent_turn","input":{"model":"local","request":{}}}}]}});
    assert!(core.register("operator", "internal", internal).is_err());
    let minimal = json!({"type":"agent","input":{"model":"local","prompt":"Use defaults"}});
    core.register("operator", "minimal", minimal).unwrap();
}
#[test]
fn scheduled_context_requires_exact_retained_definition_after_native_delete() {
    let (_home, core) = fixture();
    core.register(
        "operator",
        "scheduled",
        json!({"type":"echo","input":{"text":"retained"}}),
    )
    .unwrap();
    let retained = core::ScheduleContext {
        home_id: core.policy.home_id.clone(),
        actor: "operator".into(),
        work_id: "scheduled".into(),
        name: "operator.retained".into(),
        params: Some(json!({"source":"retained"})),
    };
    let raw = serde_json::to_string(&retained).unwrap();
    core.store
        .write(|db| {
            db.execute(
                "INSERT INTO forge_schedules(name,context,cron,tz) VALUES(?1,?2,?3,?4)",
                rusqlite::params![retained.name, raw, "0 * * * * *", "UTC"],
            )?;
            Ok(())
        })
        .unwrap();
    // The native schedule is absent: this represents an already-queued authentic
    // occurrence retained after delete, whose immutable Forge context remains valid.
    core.validate_scheduled_context(&retained).unwrap();
    let mut forged = retained.clone();
    forged.params = Some(json!({"source":"forged"}));
    assert!(core.validate_scheduled_context(&forged).is_err());
}
#[test]
fn plugin_recovery_uses_unresolved_effect_identity_to_kill_detached_tree() {
    let (home, core) = fixture();
    let effect_key = "forge-effect-v1-plugin-recovery";
    core.store
        .write(|db| {
            db.execute(
                "INSERT INTO dbos_effect_receipts(effect_key,dbos_id,invocation_digest,action_digest,state) VALUES(?1,'run','invocation','action','uncertain')",
                [effect_key],
            )?;
            Ok(())
        })
        .unwrap();
    let pid_file = home.path().join("plugin-recovery.pid");
    let invocation = crate::plugin::invocation_id(effect_key);
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "setsid sleep 60 & echo $! > \"$1\"; wait", "fixture"])
        .arg(&pid_file)
        .env("FORGE_INVOCATION_ID", &invocation)
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if pid_file.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    struct PluginCleanup(String);
    impl Drop for PluginCleanup {
        fn drop(&mut self) {
            if !self.0.is_empty() {
                let _ = crate::containment::reconcile_invocation(&self.0);
            }
        }
    }
    let mut cleanup = PluginCleanup(invocation.clone());
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    core.reconcile_plugin_effects().unwrap();
    cleanup.0.clear();
    let _ = child.wait();
    assert!(
        !crate::containment::alive(&crate::plugin::invocation_id(effect_key), None, None).unwrap(),
        "unresolved plugin effect left its detached invocation alive: {pid}"
    );
}
#[tokio::test]
async fn distinct_structured_slots_retain_independent_effects_and_verified_replay() {
    let (home, core) = fixture();
    let e = admitted(&core, "slots", None);
    let action = json!({"type":"plugin.record","input":{"value":"retained"}});
    let first = core
        .flow_effect(&e, "root/step:a.b", &action)
        .await
        .unwrap();
    let second = core
        .flow_effect(&e, "root/step:a/step:b", &action)
        .await
        .unwrap();
    assert_eq!(first["count"], 1);
    assert_eq!(second["count"], 2);
    assert_eq!(
        core.flow_effect(&e, "root/step:a.b", &action)
            .await
            .unwrap(),
        first
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("count")).unwrap(),
        "2"
    );
    core.flow_verify_result(&e, "root/step:a.b", &action, &first)
        .unwrap();
    assert!(
        core.flow_verify_result(&e, "root/step:a.b", &action, &second)
            .is_err()
    );
    core.fence_flow(&e).unwrap();
    assert!(
        core.flow_effect(&e, "root/step:later", &action)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("count")).unwrap(),
        "2"
    );
}
#[tokio::test]
async fn uncertain_flow_effect_never_resends_and_reconciliation_fences_future_slots() {
    let (home, core) = fixture();
    let e = admitted(&core, "unknown", None);
    let action = json!({"type":"plugin.record","input":{"value":"unknown","fail":true}});
    assert!(
        core.flow_effect(&e, "root/step:unknown", &action)
            .await
            .is_err()
    );
    assert!(
        core.flow_effect(&e, "root/step:unknown", &action)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("count")).unwrap(),
        "1"
    );
    assert!(
        core.reconcile_flow(
            &e,
            Some("wrong"),
            "not_applied",
            "provider settled",
            Value::Null
        )
        .is_err()
    );
    core.reconcile_flow(
        &e,
        Some("root/step:unknown"),
        "not_applied",
        "provider settled",
        Value::Null,
    )
    .unwrap();
    let effects = core.flow_effect_observations(&e).unwrap();
    assert_eq!(effects[0]["state"], "not_applied");
    assert_eq!(effects[0]["reconciliation"]["note"], "provider settled");
    assert!(
        core.flow_effect(&e, "root/step:later", &action)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("count")).unwrap(),
        "1"
    );
}
#[tokio::test]
async fn flow_receipt_tampering_prevents_cached_publication() {
    let (home, core) = fixture();
    let e = admitted(&core, "tamper", None);
    let action = json!({"type":"plugin.record","input":{"value":"retained"}});
    let output = core
        .flow_effect(&e, "root/step:one", &action)
        .await
        .unwrap();
    let path = home.path().join("state/outputs").join(format!(
        "flow-{}.json",
        hash(serde_json::to_vec(&[e.dbos_id.as_str(), "root/step:one"]).unwrap())
    ));
    std::fs::write(&path, b"{}").unwrap();
    assert!(
        core.flow_verify_result(&e, "root/step:one", &action, &output)
            .is_err()
    );
    assert!(
        core.flow_effect(&e, "root/step:one", &action)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("count")).unwrap(),
        "1"
    );
    assert!(safety::read_private(&path).is_ok());
}
fn reopen(home: &std::path::Path) -> Arc<Core> {
    let policy = core::load_policy(home).unwrap();
    let config = crate::settings::Settings::for_tests(&policy)
        .runtime_config(&policy)
        .unwrap();
    let store = Arc::new(
        AdmissionStore::initialize(&crate::settings::Paths::defaults(home), &policy.home_id)
            .unwrap(),
    );
    Core::new(
        crate::settings::Paths::defaults(home),
        policy,
        config,
        store,
        BTreeMap::new(),
    )
    .unwrap()
}
fn admissions(core: &Core) -> i64 {
    core.store
        .write(|db| Ok(db.query_row("SELECT count(*) FROM dbos_invocations", [], |r| r.get(0))?))
        .unwrap()
}
/// A scheduled handoff that fails after durable admission must not hide that run:
/// the occurrence resolves to the same single admission across restart. A refusal
/// before admission leaves the occurrence with no run.
#[tokio::test]
async fn scheduled_occurrence_resolves_existing_admission_independent_of_handoff_outcome() {
    use crate::dbos::admission::SubmissionState;
    use std::time::{Duration, UNIX_EPOCH};
    let (home, core) = fixture();
    core.register(
        "operator",
        "tick",
        json!({"type":"echo","input":{"text":"due"}}),
    )
    .unwrap();
    let context = core::ScheduleContext {
        home_id: core.policy.home_id.clone(),
        actor: "operator".into(),
        work_id: "tick".into(),
        name: "forge-tick-occurrence".into(),
        params: None,
    };
    let retained = serde_json::to_string(&context).unwrap();
    core.store
        .write(|db| {
            db.execute(
                "INSERT INTO forge_schedules(name,context,cron,tz) VALUES(?1,?2,'*/15 * * * * *','UTC')",
                [&context.name, &retained],
            )?;
            Ok(())
        })
        .unwrap();
    let due = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    assert_eq!(core.scheduled_admission(&context, due).unwrap(), None);
    // Fault fixture: the home's audit append fails after admission has committed.
    core.store
        .write(|db| {
            db.execute_batch("CREATE TRIGGER fixture_audit_fault BEFORE INSERT ON forge_audit WHEN NEW.action='run' BEGIN SELECT RAISE(ABORT,'fixture audit unavailable'); END;")?;
            Ok(())
        })
        .unwrap();
    let failed = core.schedule_handoff(&context, due).await.unwrap_err();
    assert!(format!("{failed:#}").contains("fixture audit unavailable"));
    let run = core
        .scheduled_admission(&context, due)
        .unwrap()
        .expect("post-admission handoff failure retains its admitted run");
    let record = core.store.inspect(&run).unwrap().unwrap();
    assert_eq!(record.state, SubmissionState::Admitted);
    assert_eq!(record.envelope.invocation.workspace(), "tick");
    assert_eq!(record.envelope.invocation.principal(), "operator");
    // Unsubmitted: maintenance's reconcile batch will submit and execute it.
    assert_eq!(core.store.remaining().unwrap(), 1);
    assert_eq!(admissions(&core), 1);
    // Another actor's identical schedule name resolves to no admission here.
    let foreign = core::ScheduleContext {
        actor: "other".into(),
        ..context.clone()
    };
    assert_eq!(core.scheduled_admission(&foreign, due).unwrap(), None);
    core.store
        .write(|db| {
            db.execute_batch("DROP TRIGGER fixture_audit_fault;")?;
            Ok(())
        })
        .unwrap();
    // Restart on the same home: the same single admission resolves, and the same
    // deterministic identity re-admits idempotently without a duplicate.
    drop(core);
    let core = reopen(home.path());
    assert_eq!(
        core.scheduled_admission(&context, due).unwrap(),
        Some(run.clone())
    );
    let again = core.store.admit(&record.envelope.invocation).unwrap();
    assert_eq!(again.envelope.dbos_id, run);
    assert_eq!(admissions(&core), 1);
    // A refusal before admission (disabled work) admits nothing for its occurrence.
    core.enable("operator", "tick", false).unwrap();
    let refused_due = due + Duration::from_secs(15);
    assert!(core.schedule_handoff(&context, refused_due).await.is_err());
    assert_eq!(
        core.scheduled_admission(&context, refused_due).unwrap(),
        None
    );
    assert_eq!(admissions(&core), 1);
    assert_eq!(core.scheduled_admission(&context, due).unwrap(), Some(run));
}

// ---- Golden digests -----------------------------------------------------------
// Every value below was produced by the 70a8f4f build (before the ACP dependencies
// unified serde_json/preserve_order). They pin the on-disk identity of recorded
// work, params, receipts and effect bindings: a change here breaks upgraded homes
// with "immutable flow effect binding changed" or an admission conflict.
fn golden_samples() -> Vec<(&'static str, String)> {
    let (home, core) = fixture();
    let _keep = home;
    let unsorted_workflow = json!({"type":"workflow","input":{"steps":[
        {"id":"b","action":{"type":"echo","input":{"text":"two","delay":0}},"needs":[]},
        {"id":"a","action":{"type":"echo","input":{"text":"${/steps/b/output/text}"}},"needs":["b"]}],
        "params":{"zeta":1,"alpha":{"y":[3,2,1],"b":null,"a":true}},
        "output":{"report":{"$ref":"/steps/a/output/text"}}}});
    let echo = json!({"type":"echo","input":{"text":"x","delay":1}});
    let agent = json!({"type":"agent","input":{"model":"local","prompt":"p","instructions":"i","tools":[],"max_rounds":2}});
    let params = json!({"zeta":{"b":2,"a":1},"alpha":[{"y":1,"x":0}]});
    let receipt = json!({"run_id":"r","invocation":"i","slot":"s","action_digest":"d","output":{"z":1,"a":{"q":[2,{"b":1,"a":0}]}}});
    let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(5);
    vec![
        (
            "action_hash_workflow",
            core.action_hash("operator", &unsorted_workflow).unwrap(),
        ),
        (
            "action_hash_echo",
            core.action_hash("operator", &echo).unwrap(),
        ),
        (
            "action_hash_agent",
            core.action_hash("operator", &agent).unwrap(),
        ),
        (
            "definition_sha256",
            hash(crate::canonical::to_vec(&unsorted_workflow).unwrap()),
        ),
        (
            "stored_definition_text",
            crate::canonical::to_string(&unsorted_workflow).unwrap(),
        ),
        (
            "params_digest",
            hash(crate::canonical::to_string(&params).unwrap()),
        ),
        (
            "receipt_sha256",
            hash(crate::canonical::to_vec(&receipt).unwrap()),
        ),
        ("request_id", core::request_id("work", "key").unwrap()),
        ("schedule_key", core::schedule_key("sched", at).unwrap()),
        (
            "occurrence_key_manual",
            core::occurrence_key("sched", at, Some("sched-sched-trigger-7")).unwrap(),
        ),
        (
            "dbos_id",
            Invocation::derive_dbos_id(
                "0123456789abcdef0123456789abcdef",
                "operator",
                core::WORKFLOW,
                "req",
            ),
        ),
        (
            "flow_effect_key",
            hash(crate::canonical::to_vec(&json!(["run", "slot", "digest"])).unwrap()),
        ),
    ]
}

#[test]
fn recorded_digests_match_the_70a8f4f_build() {
    let expected = [
        (
            "action_hash_workflow",
            "0fb3a7b559a3de0631ab52cecce341150a543a8aecd1ac064d2f9c6d781fe13c",
        ),
        (
            "action_hash_echo",
            "d9668b96a451609737ce590e91be01b46f34b8446d65166bd516f9b944683851",
        ),
        (
            "action_hash_agent",
            "a7f8f6640ad8f015b65e06b38f1b494fbbb4f83150b1605dea52264fead1f9be",
        ),
        (
            "definition_sha256",
            "f501630042b5d58abbac83aabc838d5080c71cc04b668dd5cf60876fa691b696",
        ),
        (
            "stored_definition_text",
            r#"{"input":{"output":{"report":{"$ref":"/steps/a/output/text"}},"params":{"alpha":{"a":true,"b":null,"y":[3,2,1]},"zeta":1},"steps":[{"action":{"input":{"delay":0,"text":"two"},"type":"echo"},"id":"b","needs":[]},{"action":{"input":{"text":"${/steps/b/output/text}"},"type":"echo"},"id":"a","needs":["b"]}]},"type":"workflow"}"#,
        ),
        (
            "params_digest",
            "706f99914bc33c46524a8900ba1673727b53f976e6114653ce83e275a8566d84",
        ),
        (
            "receipt_sha256",
            "d24b757d7bb9a95a17329052cde3c24d7a9ddddeaeebee8165b6e9843912d452",
        ),
        (
            "request_id",
            "002f96351d0f2693490706f42625fa4e4ec3dca79d126b8868bfaa3a49d98f07",
        ),
        (
            "schedule_key",
            "f957ddda15f29f93414e69351949fdfd1232d1484e4b15bf256912496e8cb9a1",
        ),
        (
            "occurrence_key_manual",
            "7382abf2e10509d60c60059bbc701af2a5e644164c6edb84dd4d4ea31366292e",
        ),
        (
            "dbos_id",
            "forge-v1-275a7fc20d6a80662c394d3cebfdfef6f6bdd91fab08329ecfe12e2accd52544",
        ),
        (
            "flow_effect_key",
            "254862cae9fc1283cde2fe498fe1fc2bb7e9aa98178bc907f95b227e27c87111",
        ),
    ];
    let actual = golden_samples();
    assert_eq!(actual.len(), expected.len());
    for ((name, value), (want_name, want)) in actual.iter().zip(expected) {
        assert_eq!(*name, want_name);
        assert_eq!(value, want, "digest {name} changed from the 70a8f4f build");
    }
}

#[test]
fn registered_definition_text_is_canonical_and_reregistration_is_idempotent() {
    let (_home, core) = fixture();
    let first = json!({"type":"echo","input":{"text":"x","delay":1}});
    core.register("operator", "w1", first.clone()).unwrap();
    let stored: String = core
        .store
        .write(|c| {
            Ok(
                c.query_row("SELECT definition FROM forge_work WHERE id='w1'", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .unwrap();
    assert_eq!(stored, r#"{"input":{"delay":1,"text":"x"},"type":"echo"}"#);
    // Same definition, different key order: still the same immutable identity.
    let reordered: Value =
        serde_json::from_str(r#"{"type":"echo","input":{"text":"x","delay":1}}"#).unwrap();
    core.register("operator", "w1", reordered).unwrap();
}

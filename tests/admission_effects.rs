//! Native SQLite boundary regression tests; real DBOS/provider process tests
//! are separately recorded by the live acceptance harness.
use agent_forge::{
    dbos::{
        admission::{AdmissionStore, Envelope, Error, Invocation, ScopedReference},
        effects::EffectAccess,
    },
    safety::{self, HomeLock},
};
use serde_json::json;
use tempfile::TempDir;

const HOME_ID: &str = "0123456789abcdef0123456789abcdef";

fn fixture() -> (TempDir, AdmissionStore) {
    let home = tempfile::tempdir().unwrap();
    safety::private_mkdir(home.path()).unwrap();
    safety::private_mkdir(&home.path().join("state")).unwrap();
    let store = AdmissionStore::initialize(
        &agent_forge::settings::Paths::defaults(home.path()),
        HOME_ID,
    )
    .unwrap();
    (home, store)
}

fn invocation(key: &str) -> Invocation {
    Invocation::from_json(
        &json!({"format_version":1,"home_id":HOME_ID,"scope":"actor:test",
        "workflow_name":"test.boundary","request_id":key,"workflow_version":"test-v1",
        "principal":"actor:test","workspace":"work:test","plugin":"plugin.test",
        "resource":"effect:test","policy_ref":"policy:test",
        "definition_sha256":"a".repeat(64),"action_sha256":"b".repeat(64),
        "parameters":{},"evidence":[],"authorization_expires_at_ms":null})
        .to_string(),
    )
    .unwrap()
}

fn admitted(store: &AdmissionStore, key: &str) -> Envelope {
    store.admit(&invocation(key)).unwrap().envelope
}

fn receipt() -> ScopedReference {
    ScopedReference {
        scope: "actor:test".into(),
        object_id: "output:test".into(),
        sha256: "c".repeat(64),
    }
}

#[test]
fn admission_same_key_is_idempotent_but_cannot_change_its_action_or_home() {
    let (_home, store) = fixture();
    let first = admitted(&store, "stable");
    assert_eq!(admitted(&store, "stable"), first);
    let mut changed = first.invocation.source_value();
    changed["action_sha256"] = json!("d".repeat(64));
    let changed = Invocation::from_json(&changed.to_string()).unwrap();
    assert_eq!(changed.dbos_id(), first.dbos_id);
    assert!(store.admit(&changed).is_err());
    let mut foreign = first.invocation.source_value();
    foreign["home_id"] = json!("f".repeat(32));
    assert!(
        store
            .admit(&Invocation::from_json(&foreign.to_string()).unwrap())
            .is_err()
    );
}

#[test]
fn abandoned_effect_intent_survives_restart_and_cannot_dispatch_twice() {
    let (home, store) = fixture();
    let envelope = admitted(&store, "uncertain");
    assert!(matches!(
        store
            .effect(
                &envelope,
                envelope.invocation.action_sha256(),
                |_, _| Ok(()),
                |_| Ok(())
            )
            .unwrap(),
        EffectAccess::Execute(_)
    ));
    drop(store);
    let reopened = AdmissionStore::initialize(
        &agent_forge::settings::Paths::defaults(home.path()),
        HOME_ID,
    )
    .unwrap();
    assert_eq!(
        reopened.effect_observation(&envelope).unwrap().unwrap()["state"],
        "uncertain"
    );
    assert!(
        reopened
            .effect(
                &envelope,
                envelope.invocation.action_sha256(),
                |_, _| Ok(()),
                |_| Ok(())
            )
            .is_err()
    );
}

#[test]
fn completed_effect_returns_receipt_and_rechecks_authority_and_integrity() {
    let (_home, store) = fixture();
    let envelope = admitted(&store, "complete");
    let EffectAccess::Execute(permit) = store
        .effect(
            &envelope,
            envelope.invocation.action_sha256(),
            |_, _| Ok(()),
            |_| Ok(()),
        )
        .unwrap()
    else {
        panic!("first call must reserve intent")
    };
    permit
        .before_outbound(|_, _| Ok(()))
        .unwrap()
        .complete(receipt(), |_| Ok(()))
        .unwrap();
    assert!(matches!(
        store
            .effect(
                &envelope,
                envelope.invocation.action_sha256(),
                |_, _| Ok(()),
                |_| Ok(())
            )
            .unwrap(),
        EffectAccess::Verified(_)
    ));
    assert!(
        store
            .effect(
                &envelope,
                envelope.invocation.action_sha256(),
                |_, _| Err(Error::Invalid("revoked")),
                |_| Ok(())
            )
            .is_err()
    );
    assert!(
        store
            .effect(
                &envelope,
                envelope.invocation.action_sha256(),
                |_, _| Ok(()),
                |_| Err(Error::Invalid("receipt changed"))
            )
            .is_err()
    );
    assert!(
        store
            .reconcile_effect(
                &envelope,
                None,
                "actor:test",
                "Provider settled; independently checked receipt"
            )
            .is_err()
    );
    assert_eq!(
        store.effect_observation(&envelope).unwrap().unwrap()["state"],
        "completed"
    );
}

#[test]
fn cancellation_before_dispatch_permanently_fences_late_original() {
    let (home, store) = fixture();
    let envelope = admitted(&store, "cancel-before-dispatch");
    store.fence_unstarted(&envelope).unwrap();
    drop(store);
    let store = AdmissionStore::initialize(
        &agent_forge::settings::Paths::defaults(home.path()),
        HOME_ID,
    )
    .unwrap();
    assert_eq!(
        store.effect_observation(&envelope).unwrap().unwrap()["state"],
        "not_applied"
    );
    assert!(
        store
            .effect(
                &envelope,
                envelope.invocation.action_sha256(),
                |_, _| Ok(()),
                |_| Ok(())
            )
            .is_err()
    );
    assert!(
        store
            .reconcile_effect(
                &envelope,
                Some(&receipt()),
                "actor:test",
                "Provider settled; independently checked receipt"
            )
            .is_err()
    );
    let replacement = admitted(&store, "replacement");
    assert!(matches!(
        store
            .effect(
                &replacement,
                replacement.invocation.action_sha256(),
                |_, _| Ok(()),
                |_| Ok(())
            )
            .unwrap(),
        EffectAccess::Execute(_)
    ));
}

#[test]
fn reconciliation_records_settled_outcome_without_resuming_original() {
    let (_home, store) = fixture();
    let envelope = admitted(&store, "settled-absent");
    drop(
        store
            .effect(
                &envelope,
                envelope.invocation.action_sha256(),
                |_, _| Ok(()),
                |_| Ok(()),
            )
            .unwrap(),
    );
    store
        .reconcile_effect(
            &envelope,
            None,
            "actor:test",
            "Provider settled; independently checked receipt",
        )
        .unwrap();
    assert_eq!(
        store.effect_decision(&envelope).unwrap().unwrap()["note"],
        "Provider settled; independently checked receipt"
    );
    assert!(
        store
            .effect(
                &envelope,
                envelope.invocation.action_sha256(),
                |_, _| Ok(()),
                |_| Ok(())
            )
            .is_err()
    );
    let other = admitted(&store, "settled-complete");
    drop(
        store
            .effect(
                &other,
                other.invocation.action_sha256(),
                |_, _| Ok(()),
                |_| Ok(()),
            )
            .unwrap(),
    );
    let mut wrong_scope = receipt();
    wrong_scope.scope = "actor:foreign".into();
    assert!(
        store
            .reconcile_effect(
                &other,
                Some(&wrong_scope),
                "actor:test",
                "Provider settled; independently checked receipt"
            )
            .is_err()
    );
    store
        .reconcile_effect(
            &other,
            Some(&receipt()),
            "actor:test",
            "Provider settled; independently checked receipt",
        )
        .unwrap();
    store.fence_unstarted(&other).unwrap(); // Never replaces existing proof.
    assert!(matches!(
        store
            .effect(
                &other,
                other.invocation.action_sha256(),
                |_, _| Ok(()),
                |_| Ok(())
            )
            .unwrap(),
        EffectAccess::Verified(_)
    ));
}

#[test]
fn authority_is_rechecked_at_actual_outbound_boundary() {
    let (_home, store) = fixture();
    let envelope = admitted(&store, "revoke-after-intent");
    let EffectAccess::Execute(permit) = store
        .effect(
            &envelope,
            envelope.invocation.action_sha256(),
            |_, _| Ok(()),
            |_| Ok(()),
        )
        .unwrap()
    else {
        panic!("intent missing")
    };
    assert!(
        permit
            .before_outbound(|_, _| Err(Error::Invalid("authority revoked")))
            .is_err()
    );
    assert_eq!(
        store.effect_observation(&envelope).unwrap().unwrap()["state"],
        "uncertain"
    );
    assert!(
        store
            .effect(&envelope, &"d".repeat(64), |_, _| Ok(()), |_| Ok(()))
            .is_err()
    );
}

#[test]
fn stale_submission_confirmation_cannot_override_newer_attempt() {
    let (_home, store) = fixture();
    let envelope = admitted(&store, "submission-gap");
    assert!(store.project(&envelope.dbos_id, "SUCCESS").is_err());
    let earlier = store.begin_submission(&envelope.dbos_id).unwrap();
    let later = store.begin_submission(&envelope.dbos_id).unwrap();
    assert!(!store.mark_submitted(&earlier).unwrap());
    assert!(store.mark_submitted(&later).unwrap());
    assert!(store.project(&envelope.dbos_id, "PENDING").is_err());
    store.project(&envelope.dbos_id, "SUCCESS").unwrap();
    store.project(&envelope.dbos_id, "SUCCESS").unwrap();
    assert!(store.project(&envelope.dbos_id, "CANCELLED").is_err());
}

#[test]
fn home_ownership_excludes_a_second_executor_and_offline_backup() {
    let (home, _store) = fixture();
    let owner = HomeLock::acquire(&agent_forge::settings::Paths::defaults(home.path())).unwrap();
    assert!(HomeLock::acquire(&agent_forge::settings::Paths::defaults(home.path())).is_err());
    drop(owner);
    assert!(HomeLock::acquire(&agent_forge::settings::Paths::defaults(home.path())).is_ok());
}

async fn dormant_queue_core(
    home: &std::path::Path,
    database_url: &str,
    callbacks: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> (
    std::sync::Arc<agent_forge::core::Core>,
    agent_forge::dbos::Runtime,
) {
    use agent_forge::{
        core::{Core, WORKFLOW, WorkFault, load_policy},
        dbos::Runtime,
        settings::{Overrides, Settings},
    };
    use std::sync::{Arc, atomic::Ordering};

    let policy = load_policy(home).unwrap();
    let settings = Settings::resolve(
        home.to_owned(),
        &policy,
        &Overrides {
            database_url: Some(database_url.to_owned()),
            ..Default::default()
        },
        &|_| None,
    )
    .unwrap();
    let config = settings.runtime_config(&policy).unwrap();
    let store = Arc::new(AdmissionStore::initialize(&settings.paths, &policy.home_id).unwrap());
    let core = Core::new(
        settings.paths.clone(),
        policy,
        config.clone(),
        Arc::clone(&store),
        Default::default(),
    )
    .unwrap();
    let mut runtime = Runtime::new(&settings.paths, config.clone()).unwrap();
    runtime.application = Some(Arc::clone(&core));
    let callbacks = Arc::clone(callbacks);
    runtime
        .register_workflow(WORKFLOW, move |_: Envelope| {
            callbacks.fetch_add(1, Ordering::SeqCst);
            async { Ok::<_, dbos::Error<WorkFault>>(serde_json::Value::Null) }
        })
        .unwrap();
    let bridge = runtime.install_submission_bridge(store).unwrap();
    assert!(core.bridge.set(bridge).is_ok());
    // Initialize the schema and submission maintenance metadata. The executor
    // and scheduler stay dormant, including when this helper reopens the home.
    runtime.prepare().await.unwrap();
    assert!(
        core.client
            .set(config.connect_client().await.unwrap())
            .is_ok()
    );
    (core, runtime)
}

/// Run with an owned, newly created loopback PostgreSQL database named
/// forge_queue_regression and role forge_queue_regression. Set
/// FORGE_QUEUE_TEST_POSTGRES to its explicit password-free connection URL.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database; no executor starts"]
async fn exact_duplicate_and_queued_cancel_survive_reopen() {
    use agent_forge::core;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let url = std::env::var("FORGE_QUEUE_TEST_POSTGRES").expect("disposable test database URL");
    let parsed = reqwest::Url::parse(&url).unwrap();
    assert_eq!(parsed.scheme(), "postgresql");
    assert_eq!(parsed.host_str(), Some("127.0.0.1"));
    assert_eq!(parsed.username(), "forge_queue_regression");
    assert_eq!(parsed.path(), "/forge_queue_regression");
    assert!(parsed.port().is_some_and(|port| port >= 1024));
    assert!(parsed.password().is_none() && parsed.query().is_none() && parsed.fragment().is_none());

    let home = tempfile::tempdir().unwrap();
    core::init(home.path()).unwrap();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let (core, runtime) = dormant_queue_core(home.path(), &url, &callbacks).await;
    let actor = "operator";
    let work = "queue-cancel-checklist";
    let key = "exact-params";
    core.register(
        actor,
        work,
        json!({"type":"workflow","input":{"steps":[
            {"id":"echo","action":{"type":"echo","input":{"text":"dormant test input"}}}
        ]}}),
    )
    .unwrap();

    let first = core
        .run_with_params(actor, work, key, None, Some(json!({})))
        .await
        .unwrap();
    let id = first["run_id"].as_str().unwrap();
    assert_eq!(
        core.run_with_params(actor, work, key, None, Some(json!({})))
            .await
            .unwrap(),
        first
    );
    for input in [None, Some(json!({"text":"changed"}))] {
        let error = core
            .run_with_params(actor, work, key, None, input)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("same ID has a different immutable invocation")
        );
    }
    let queued = core.queues(actor, Some("work"), None).await.unwrap();
    let [member] = queued["queues"][0]["members"]
        .as_array()
        .unwrap()
        .as_slice()
    else {
        panic!("expected the one queued identity");
    };
    assert_eq!(member["run_id"], id);
    assert_eq!(member["state"], "waiting");
    assert_eq!(member["actions"]["cancel"], true);
    assert_eq!(core.inspect(actor, id).await.unwrap()["status"], "ENQUEUED");

    let terminal = core.cancel(actor, id).await.unwrap();
    assert_eq!(terminal["status"], "CANCELLED");
    assert_eq!(terminal["actions"]["cancel"], false);
    assert_eq!(terminal["output"], serde_json::Value::Null);
    assert_eq!(terminal["steps"], json!([]));
    assert_eq!(terminal["effects"], json!([]));
    assert_eq!(
        terminal["logs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["action"] == "cancel")
            .count(),
        1
    );
    assert_eq!(core.cancel(actor, id).await.unwrap(), terminal);
    let envelope = core.store.inspect(id).unwrap().unwrap().envelope;
    core.client.get().unwrap().close().await;
    drop(runtime);
    drop(core);

    let (core, runtime) = dormant_queue_core(home.path(), &url, &callbacks).await;
    assert_eq!(
        core.check_consistency().await.unwrap(),
        json!({"lost":[],"orphans":[]})
    );
    assert_eq!(
        core.run_with_params(actor, work, key, None, Some(json!({})))
            .await
            .unwrap(),
        first
    );
    let record = core.store.inspect(id).unwrap().unwrap();
    assert_eq!(record.envelope, envelope);
    assert_eq!(record.runtime_status.as_deref(), Some("CANCELLED"));
    let reopened = core.inspect(actor, id).await.unwrap();
    assert_eq!(reopened["status"], terminal["status"]);
    assert_eq!(reopened["output"], terminal["output"]);
    assert_eq!(reopened["steps"], terminal["steps"]);
    assert_eq!(reopened["effects"], terminal["effects"]);
    assert_eq!(reopened["actions"], terminal["actions"]);
    assert_eq!(core.cancel(actor, id).await.unwrap(), reopened);

    let queue_after = core.queues(actor, Some("work"), None).await.unwrap();
    assert_eq!(queue_after["queues"][0]["members"], json!([]));
    assert_eq!(queue_after["admitted_not_on_queue"]["yours"], json!([]));
    let native = core.client.get().unwrap();
    let rows = native.list_workflows(&Default::default()).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].workflow_id, id);
    assert_eq!(rows[0].status.as_str(), "CANCELLED");
    assert!(native.list_workflow_steps(id).await.unwrap().is_empty());
    assert_eq!(callbacks.load(Ordering::SeqCst), 0);

    let db = rusqlite::Connection::open_with_flags(
        core.store.path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    for (table, expected) in [
        ("dbos_invocations", 1),
        ("forge_flow_effects", 0),
        ("dbos_effect_receipts", 0),
    ] {
        let count: i64 = db
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, expected, "{table}");
    }
    println!(
        "QUEUE_RUN_FACTS={}",
        json!({
            "run_id":id,"before_queue":queued,"after_queue":queue_after,"terminal":reopened,
            "admission_count":1,"dispatch_count":0,"native_workflow_count":1,"effect_receipt_count":0,
        })
    );
    native.close().await;
    drop(runtime);
}

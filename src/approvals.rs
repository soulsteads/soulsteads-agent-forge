//! Durable approval gates: an `approval` workflow step waits on a native DBOS message
//! until an authorized actor approves or rejects it (or an optional timeout passes).
//!
//! The SQLite `forge_approvals` row is the single source of truth for the decision and
//! the dedup guard (one row per run and step, decided by one atomic conditional update).
//! The DBOS message only wakes the waiting workflow; it is sent with an idempotency key
//! and redelivered after a restart if a crash fell between the decision and the send. The
//! waiting workflow is a pending DBOS workflow, so DBOS recovery resumes it after a
//! restart with its remaining timeout. While it waits it occupies a work-queue slot, like
//! a durable sleep does.
use crate::{
    core::{Core, Denied, Invalid, hash},
    dbos::admission::Envelope,
};
use anyhow::{Result, bail};
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

pub const MAX_TIMEOUT_SECONDS: u64 = 7 * 24 * 3600;
const LIST_LIMIT: usize = 100;
const NOTE_LIMIT: usize = 1024;

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS forge_approvals(id TEXT PRIMARY KEY,run_id TEXT NOT NULL,slot TEXT NOT NULL,owner TEXT NOT NULL,title TEXT NOT NULL,description TEXT NOT NULL,state TEXT NOT NULL,requested INTEGER NOT NULL,expires INTEGER,decided INTEGER,actor TEXT,note TEXT,delivered INTEGER NOT NULL DEFAULT 0,UNIQUE(run_id,slot));";

pub fn topic(id: &str) -> String {
    format!("approval:{id}")
}
fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}
fn row_json(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "approval_id": r.get::<_, String>(0)?,
        "run_id": r.get::<_, String>(1)?,
        "step": r.get::<_, String>(2)?,
        "owner": r.get::<_, String>(3)?,
        "title": r.get::<_, String>(4)?,
        "description": r.get::<_, String>(5)?,
        "state": r.get::<_, String>(6)?,
        "requested_ms": r.get::<_, i64>(7)?,
        "expires_ms": r.get::<_, Option<i64>>(8)?,
        "decided_ms": r.get::<_, Option<i64>>(9)?,
        "actor": r.get::<_, Option<String>>(10)?,
        "note": r.get::<_, Option<String>>(11)?,
        "delivered": r.get::<_, i64>(12)? == 1,
    }))
}
const COLUMNS: &str =
    "id,run_id,slot,owner,title,description,state,requested,expires,decided,actor,note,delivered";

/// Authored `approval` step input, validated before registration.
pub fn validate_input(input: &serde_json::Map<String, Value>) -> Result<()> {
    for key in input.keys() {
        if !["title", "description", "timeout_seconds"].contains(&key.as_str()) {
            bail!("approval input has an unsupported field");
        }
    }
    let text = |key: &str, required: bool, max: usize| -> Result<()> {
        match input.get(key) {
            Some(Value::String(s)) if s.len() <= max && (!required || !s.trim().is_empty()) => {
                Ok(())
            }
            None if !required => Ok(()),
            _ => bail!("approval {key} must be a bounded string"),
        }
    };
    text("title", true, 200)?;
    text("description", false, 4096)?;
    if let Some(t) = input.get("timeout_seconds")
        && !t
            .as_u64()
            .is_some_and(|t| (1..=MAX_TIMEOUT_SECONDS).contains(&t))
    {
        bail!("approval timeout_seconds must be 1 to {MAX_TIMEOUT_SECONDS}");
    }
    Ok(())
}

impl Core {
    pub(crate) fn approval_id(run_id: &str, slot: &str) -> String {
        hash(crate::canonical::to_vec(&json!(["approval", run_id, slot])).expect("tuple"))[..32]
            .to_owned()
    }
    /// Idempotently record the pending request of one step of one run.
    pub(crate) fn approval_request(
        &self,
        e: &Envelope,
        slot: &str,
        input: &Value,
    ) -> Result<String> {
        let id = Self::approval_id(&e.dbos_id, slot);
        let requested = now_ms();
        let expires = input["timeout_seconds"]
            .as_u64()
            .map(|t| requested + (t as i64) * 1000);
        let created = self.store.write(|c| {
            Ok(c.execute(
                "INSERT OR IGNORE INTO forge_approvals(id,run_id,slot,owner,title,description,state,requested,expires) VALUES(?1,?2,?3,?4,?5,?6,'pending',?7,?8)",
                params![
                    id,
                    e.dbos_id,
                    slot,
                    e.invocation.principal(),
                    input["title"].as_str().unwrap_or(""),
                    input["description"].as_str().unwrap_or(""),
                    requested,
                    expires
                ],
            )?)
        })?;
        if created == 1 {
            self.record(
                "runtime",
                "approval_requested",
                &e.dbos_id,
                &format!("{slot}:{id}"),
            )?;
        }
        Ok(id)
    }
    fn approval_row(&self, id: &str) -> Result<Option<Value>> {
        Ok(self.store.write(|c| {
            Ok(c.query_row(
                &format!("SELECT {COLUMNS} FROM forge_approvals WHERE id=?1"),
                [id],
                row_json,
            )
            .optional()?)
        })?)
    }
    /// What a woken (or timed-out) gate does: the row decides. Pending after the wait
    /// ended means the timeout passed, which is recorded as a decision of its own.
    pub(crate) fn approval_finalize(&self, id: &str) -> std::result::Result<Value, String> {
        let claimed = self
            .store
            .write(|c| {
                Ok(c.execute(
                    "UPDATE forge_approvals SET state='timed_out',decided=?2,actor='runtime',note='timed out' WHERE id=?1 AND state='pending'",
                    params![id, now_ms()],
                )?)
            })
            .map_err(|_| "approval state unavailable".to_owned())?;
        let row = self
            .approval_row(id)
            .ok()
            .flatten()
            .ok_or_else(|| "approval record missing".to_owned())?;
        if claimed == 1 {
            let _ = self.record(
                "runtime",
                "approval_timed_out",
                row["run_id"].as_str().unwrap_or(""),
                id,
            );
        }
        match row["state"].as_str() {
            Some("approved") => Ok(json!({
                "approval_id": id,
                "decision": "approved",
                "actor": row["actor"],
                "note": row["note"],
                "decided_ms": row["decided_ms"],
            })),
            Some("rejected") => Err(format!(
                "approval rejected by {}: {}",
                row["actor"].as_str().unwrap_or("?"),
                row["note"].as_str().unwrap_or("")
            )),
            _ => Err("approval timed out".to_owned()),
        }
    }

    /// Approvals owned by `actor`, newest first. A pending approval whose run has ended
    /// is reported `actionable: false` with the run's status.
    pub async fn approvals(
        &self,
        actor: &str,
        state: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Value> {
        self.permit(actor, "read", None)?;
        if state.is_some_and(|s| !["pending", "approved", "rejected", "timed_out"].contains(&s)) {
            return Err(Invalid("state must be pending, approved, rejected or timed_out").into());
        }
        let limit = limit.unwrap_or(50).clamp(1, LIST_LIMIT as i64);
        let mut rows = self.store.write(|c| {
            let mut q = c.prepare(&format!(
                "SELECT {COLUMNS} FROM forge_approvals WHERE owner=?1 AND (?2 IS NULL OR state=?2) ORDER BY requested DESC, rowid DESC LIMIT ?3"
            ))?;
            Ok(q.query_map(params![actor, state, limit], row_json)?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        let pending: Vec<String> = rows
            .iter()
            .filter(|r| r["state"] == "pending")
            .filter_map(|r| r["run_id"].as_str().map(str::to_owned))
            .collect();
        let mut ended = std::collections::BTreeMap::new();
        if let (false, Some(client)) = (pending.is_empty(), self.client.get()) {
            let ids: Vec<&str> = pending.iter().map(String::as_str).collect();
            for r in client
                .list_workflows(&dbos::sysdb::types::WorkflowFilter {
                    workflow_ids: ids,
                    load_input: false,
                    load_output: false,
                    ..Default::default()
                })
                .await?
            {
                ended.insert(r.workflow_id.clone(), r.status.as_str().to_owned());
            }
        }
        for row in &mut rows {
            let status = ended.get(row["run_id"].as_str().unwrap_or(""));
            row["run_status"] = json!(status);
            row["actionable"] = json!(
                row["state"] == "pending"
                    && !status
                        .is_some_and(|s| !["PENDING", "ENQUEUED", "DELAYED"].contains(&s.as_str()))
            );
        }
        Ok(json!({"approvals":rows,"pending":rows.iter().filter(|r|r["state"]=="pending").count()}))
    }

    /// Approve or reject one pending approval. Only the owning actor with the
    /// `approve` capability may decide it, exactly once; repeating the identical
    /// decision is accepted and only retries delivery.
    pub async fn approval_decide(
        &self,
        actor: &str,
        id: &str,
        decision: &str,
        note: &str,
    ) -> Result<Value> {
        self.permit(actor, "approve", None)?;
        let state = match decision {
            "approve" => "approved",
            "reject" => "rejected",
            _ => return Err(Invalid("decision must be approve or reject").into()),
        };
        if note.trim().is_empty() || note.chars().count() > NOTE_LIMIT {
            return Err(Invalid("a decision note of 1 to 1024 characters is required").into());
        }
        let row = self
            .approval_row(id)?
            .filter(|r| r["owner"] == actor)
            .ok_or(Denied("approval missing or outside actor scope"))?;
        if row["state"] == "pending" {
            let live = if let Some(client) = self.client.get() {
                client
                    .list_workflows(&dbos::sysdb::types::WorkflowFilter {
                        workflow_ids: vec![row["run_id"].as_str().unwrap_or("")],
                        load_input: false,
                        load_output: false,
                        ..Default::default()
                    })
                    .await?
                    .first()
                    .is_none_or(|r| ["PENDING", "ENQUEUED", "DELAYED"].contains(&r.status.as_str()))
            } else {
                true
            };
            if !live {
                return Err(Invalid("the run is no longer waiting for this approval").into());
            }
            let changed = self.store.write(|c| {
                Ok(c.execute(
                    "UPDATE forge_approvals SET state=?2,decided=?3,actor=?4,note=?5 WHERE id=?1 AND state='pending'",
                    params![id, state, now_ms(), actor, note],
                )?)
            })?;
            if changed == 1 {
                self.record(
                    actor,
                    &format!("approval_{state}"),
                    row["run_id"].as_str().unwrap_or(""),
                    &format!("{id}: {note}"),
                )?;
            }
        }
        let current = self
            .approval_row(id)?
            .ok_or(Invalid("approval record missing"))?;
        if current["state"] != state || current["actor"] != actor || current["note"] != note {
            return Err(Invalid("approval was already decided; it cannot be decided again").into());
        }
        let delivered = self.approval_deliver(id).await.unwrap_or(false);
        let mut out = self.approval_row(id)?.unwrap_or(current);
        out["delivery"] = json!(if delivered || out["delivered"] == true {
            "delivered"
        } else {
            "pending_retry"
        });
        Ok(out)
    }

    /// Wake the waiting workflow once (idempotent message key); mark the row delivered.
    async fn approval_deliver(&self, id: &str) -> Result<bool> {
        let Some(row) = self.approval_row(id)? else {
            return Ok(false);
        };
        if row["delivered"] == true
            || !["approved", "rejected"].contains(&row["state"].as_str().unwrap_or(""))
        {
            return Ok(row["delivered"] == true);
        }
        let Some(client) = self.client.get() else {
            return Ok(false);
        };
        let run_id = row["run_id"].as_str().unwrap_or("");
        let message = json!({"approval_id":id,"decision":row["state"]});
        let topic = topic(id);
        let key = format!("approval-{id}");
        let sent = client
            .send_with(
                dbos::SendMessage {
                    destination_id: run_id,
                    message: &message,
                    topic: Some(&topic),
                    idempotency_key: Some(&key),
                },
                dbos::SendOptions::default(),
            )
            .await;
        if sent.is_ok() {
            self.store.write(|c| {
                c.execute("UPDATE forge_approvals SET delivered=1 WHERE id=?1", [id])?;
                Ok(())
            })?;
        }
        Ok(sent.is_ok())
    }

    /// After a restart: resend any decision whose wake-up message may have been lost.
    pub async fn approvals_redeliver(&self) -> Result<usize> {
        let ids = self.store.write(|c| {
            let mut q = c.prepare(
                "SELECT id FROM forge_approvals WHERE delivered=0 AND state IN ('approved','rejected')",
            )?;
            Ok(q.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        let mut sent = 0;
        for id in ids {
            if self.approval_deliver(&id).await.unwrap_or(false) {
                sent += 1;
            }
        }
        Ok(sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{self, Core};
    use crate::dbos::admission::{AdmissionStore, Invocation};
    use crate::settings::Paths;
    use std::{collections::BTreeMap, sync::Arc};

    fn core() -> (tempfile::TempDir, Arc<Core>) {
        let home = tempfile::tempdir().unwrap();
        core::init(home.path()).unwrap();
        let mut policy = core::load_policy(home.path()).unwrap();
        policy.actors.insert(
            "viewer".into(),
            core::Actor {
                token: "v".repeat(32),
                capabilities: vec!["read".into(), "execute".into()],
                types: vec!["approval".into()],
            },
        );
        let config = crate::settings::Settings::for_tests(&policy)
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
    fn envelope(core: &Core, key: &str) -> Envelope {
        let definition = json!({"type":"echo","input":{"text":"x"}});
        core.register("operator", "w", definition.clone()).ok();
        let invocation = Invocation::from_json(&json!({"format_version":1,"home_id":core.policy.home_id,"scope":"operator","workflow_name":core::WORKFLOW,"request_id":key,"workflow_version":core::VERSION,"principal":"operator","workspace":"w","plugin":"echo","resource":"bounded-work-queue","policy_ref":"configured-actor","definition_sha256":hash(crate::canonical::to_vec(&definition).unwrap()),"action_sha256":core.action_hash("operator",&definition).unwrap(),"parameters":{},"evidence":[],"authorization_expires_at_ms":null}).to_string()).unwrap();
        core.store.admit(&invocation).unwrap().envelope
    }

    #[test]
    fn input_validation_is_closed_and_bounded() {
        let ok = json!({"title":"Ship it","timeout_seconds":60});
        assert!(validate_input(ok.as_object().unwrap()).is_ok());
        for bad in [
            json!({}),
            json!({"title":" "}),
            json!({"title":"x","timeout_seconds":0}),
            json!({"title":"x","timeout_seconds":MAX_TIMEOUT_SECONDS + 1}),
            json!({"title":"x","extra":1}),
            json!({"title":5}),
        ] {
            assert!(validate_input(bad.as_object().unwrap()).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn one_decision_is_recorded_exactly_once_and_dedups() {
        let (_home, core) = core();
        let e = envelope(&core, "run-a");
        let id = core
            .approval_request(
                &e,
                "root/step:gate",
                &json!({"title":"Gate","timeout_seconds":30}),
            )
            .unwrap();
        // The same request is idempotent: one row, and the id is deterministic.
        assert_eq!(
            id,
            core.approval_request(&e, "root/step:gate", &json!({"title":"Gate"}))
                .unwrap()
        );
        let list = core
            .approvals("operator", Some("pending"), None)
            .await
            .unwrap();
        assert_eq!(list["pending"], 1);
        assert_eq!(list["approvals"][0]["title"], "Gate");
        assert_eq!(list["approvals"][0]["actionable"], true);
        // A decision needs a note, the approve capability and ownership.
        assert!(
            core.approval_decide("operator", &id, "approve", " ")
                .await
                .is_err()
        );
        assert!(
            core.approval_decide("operator", &id, "maybe", "n")
                .await
                .is_err()
        );
        assert!(
            core.approval_decide("viewer", &id, "approve", "n")
                .await
                .is_err()
        );
        let first = core
            .approval_decide("operator", &id, "approve", "looks fine")
            .await
            .unwrap();
        assert_eq!(first["state"], "approved");
        assert_eq!(first["actor"], "operator");
        // No executor here: delivery is pending and retried, never lost.
        assert_eq!(first["delivery"], "pending_retry");
        // Deduplication: a conflicting or differently noted decision is refused; the
        // identical one is accepted without changing anything.
        assert!(
            core.approval_decide("operator", &id, "reject", "looks fine")
                .await
                .is_err()
        );
        assert!(
            core.approval_decide("operator", &id, "approve", "other note")
                .await
                .is_err()
        );
        assert!(
            core.approval_decide("operator", &id, "approve", "looks fine")
                .await
                .is_ok()
        );
        assert_eq!(
            core.approvals("operator", Some("pending"), None)
                .await
                .unwrap()["pending"],
            0
        );
        // A woken gate reads the row, not the message.
        let out = core.approval_finalize(&id).unwrap();
        assert_eq!(
            (out["decision"].as_str(), out["actor"].as_str()),
            (Some("approved"), Some("operator"))
        );
        // Audited with the actor.
        let audit: i64 = core
            .store
            .write(|c| Ok(c.query_row("SELECT count(*) FROM forge_audit WHERE action='approval_approved' AND actor='operator'", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(audit, 1);
    }

    #[tokio::test]
    async fn rejection_timeout_and_scope_are_recorded() {
        let (_home, core) = core();
        let e = envelope(&core, "run-b");
        let id = core
            .approval_request(&e, "s", &json!({"title":"T"}))
            .unwrap();
        core.approval_decide("operator", &id, "reject", "not ready")
            .await
            .unwrap();
        let error = core.approval_finalize(&id).unwrap_err();
        assert!(error.contains("rejected by operator: not ready"));
        // An unanswered gate that wakes on its timeout records the timeout once and
        // refuses any later decision.
        let e2 = envelope(&core, "run-c");
        let id2 = core
            .approval_request(&e2, "s", &json!({"title":"T","timeout_seconds":1}))
            .unwrap();
        assert_eq!(
            core.approval_finalize(&id2).unwrap_err(),
            "approval timed out"
        );
        assert!(
            core.approval_decide("operator", &id2, "approve", "late")
                .await
                .is_err()
        );
        let all = core.approvals("operator", None, None).await.unwrap();
        let states: Vec<_> = all["approvals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["state"].as_str().unwrap())
            .collect();
        assert!(states.contains(&"rejected") && states.contains(&"timed_out"));
        // Another actor sees nothing and cannot decide.
        assert_eq!(
            core.approvals("viewer", None, None).await.unwrap()["approvals"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert!(
            core.approval_decide("viewer", &id, "approve", "n")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_pending_approval_survives_a_core_restart() {
        let home = tempfile::tempdir().unwrap();
        core::init(home.path()).unwrap();
        let make = || {
            let policy = core::load_policy(home.path()).unwrap();
            let config = crate::settings::Settings::for_tests(&policy)
                .runtime_config(&policy)
                .unwrap();
            let store = Arc::new(
                AdmissionStore::initialize(&Paths::defaults(home.path()), &policy.home_id).unwrap(),
            );
            Core::new(
                Paths::defaults(home.path()),
                policy,
                config,
                store,
                BTreeMap::new(),
            )
            .unwrap()
        };
        let first = make();
        let e = envelope(&first, "run-d");
        let id = first
            .approval_request(&e, "s", &json!({"title":"Durable"}))
            .unwrap();
        drop(first);
        let second = make();
        let pending = second
            .approvals("operator", Some("pending"), None)
            .await
            .unwrap();
        assert_eq!(pending["approvals"][0]["approval_id"], id);
        assert_eq!(second.approvals_redeliver().await.unwrap(), 0);
        second
            .approval_decide("operator", &id, "approve", "after restart")
            .await
            .unwrap();
        // Redelivery of an undelivered decision is attempted at startup (no executor here).
        assert_eq!(second.approvals_redeliver().await.unwrap(), 0);
    }

    #[test]
    fn an_approval_step_registers_in_a_workflow_but_not_as_standalone_work() {
        let (_home, core) = core();
        let flow = json!({"type":"workflow","input":{"steps":[
            {"id":"prepare","action":{"type":"echo","input":{"text":"x"}}},
            {"id":"gate","needs":["prepare"],"action":{"type":"approval","input":{"title":"Ship","description":"d","timeout_seconds":3600}}},
            {"id":"publish","needs":["gate","prepare"],"action":{"type":"echo","input":{"text":"done ${/steps/prepare/output/text}"}}}],
            "output":{"$ref":"/steps/publish/output/text"}}});
        core.register("operator", "flow", flow).unwrap();
        assert!(
            core.register(
                "operator",
                "alone",
                json!({"type":"approval","input":{"title":"x"}})
            )
            .is_err()
        );
        let bad = json!({"type":"workflow","input":{"steps":[{"id":"g","action":{"type":"approval","input":{"title":"x","timeout_seconds":0}}}]}});
        assert!(core.register("operator", "bad", bad).is_err());
    }
}

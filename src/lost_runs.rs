//! Consistency between Forge's admission records and the native DBOS workflow table.
//!
//! An admission the store records as submitted whose workflow row does not exist
//! (a restored or otherwise inconsistent database) is never silently replayed: it is
//! marked here, reported as RECOVERY_REQUIRED, and settled only by an explicit
//! operator reconciliation (`completed` -> SUCCESS with the evidence given, `not_applied`
//! -> ERROR, which then permits an explicit linked retry). Conversely, native workflows
//! with no admission record are reported and audited, never run on Forge's behalf.
use crate::core::Core;
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS forge_lost_runs(run_id TEXT PRIMARY KEY,detected REAL NOT NULL,decision TEXT,note TEXT,output TEXT,actor TEXT,decided REAL);";

/// A lost run's settlement, when one is recorded.
pub(crate) struct Lost {
    pub decision: Option<String>,
    pub output: Value,
}

impl Core {
    pub(crate) fn lost_run(&self, id: &str) -> Result<Option<Lost>> {
        Ok(self.store.write(|c| {
            Ok(c.query_row(
                "SELECT decision,output FROM forge_lost_runs WHERE run_id=?1",
                [id],
                |r| {
                    Ok(Lost {
                        decision: r.get(0)?,
                        output: r
                            .get::<_, Option<String>>(1)?
                            .and_then(|t| serde_json::from_str(&t).ok())
                            .unwrap_or(Value::Null),
                    })
                },
            )
            .optional()?)
        })?)
    }

    /// Compare admissions with native workflows. Records new lost runs and returns
    /// `{lost:[run ids], orphans:[native workflow ids with no admission]}`.
    pub async fn check_consistency(&self) -> Result<Value> {
        let client = self.native()?;
        let lost = missing_natives(client, self.store.submitted_ids()?).await?;
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.;
        for id in &lost {
            let inserted = self.store.write(|c| {
                Ok(c.execute(
                    "INSERT OR IGNORE INTO forge_lost_runs(run_id,detected) VALUES(?1,?2)",
                    rusqlite::params![id, now],
                )?)
            })?;
            if inserted == 1 {
                self.record(
                    "runtime",
                    "run_lost",
                    id,
                    "admission has no native workflow; reconcile it",
                )?;
            }
        }
        // Native workflows of this application that no admission accounts for.
        let mut orphans = Vec::new();
        let pending = client
            .list_workflows(&dbos::sysdb::types::WorkflowFilter {
                names: vec![crate::core::WORKFLOW, crate::core::SCHEDULED],
                status: vec![
                    dbos::sysdb::types::WorkflowStatus::Pending,
                    dbos::sysdb::types::WorkflowStatus::Enqueued,
                ],
                load_input: false,
                load_output: false,
                ..Default::default()
            })
            .await?;
        for row in pending {
            if row.workflow_id.starts_with("sched-") {
                continue;
            }
            if self.store.inspect(&row.workflow_id)?.is_none() {
                self.record(
                    "runtime",
                    "orphan_native_workflow",
                    &row.workflow_id,
                    "no Forge admission; not run by Forge",
                )?;
                orphans.push(row.workflow_id);
            }
        }
        Ok(json!({"lost":lost,"orphans":orphans}))
    }

    /// Settle a lost run with explicit operator evidence.
    pub(crate) fn reconcile_lost(
        &self,
        actor: &str,
        id: &str,
        decision: &str,
        note: &str,
        output: &Value,
    ) -> Result<()> {
        let changed = self.store.write(|c| {
            Ok(c.execute(
                "UPDATE forge_lost_runs SET decision=?2,note=?3,output=?4,actor=?5,decided=?6 WHERE run_id=?1 AND decision IS NULL",
                rusqlite::params![id, decision, note, output.to_string(), actor, chrono::Utc::now().timestamp_millis() as f64 / 1000.],
            )?)
        })?;
        if changed != 1 {
            return Err(crate::core::Invalid("lost run is already reconciled").into());
        }
        self.record(actor, "reconcile", id, &format!("{decision}: {note}"))?;
        Ok(())
    }
}

/// The confirmed-submitted admissions among `ids` whose native workflow row is missing.
pub(crate) async fn missing_natives(
    client: &dbos::Client,
    ids: Vec<String>,
) -> Result<Vec<String>> {
    let mut lost = Vec::new();
    for chunk in ids.chunks(100) {
        let present: std::collections::BTreeSet<String> = client
            .list_workflows(&dbos::sysdb::types::WorkflowFilter {
                workflow_ids: chunk.iter().map(String::as_str).collect(),
                load_input: false,
                load_output: false,
                ..Default::default()
            })
            .await?
            .into_iter()
            .map(|r| r.workflow_id)
            .collect();
        lost.extend(chunk.iter().filter(|id| !present.contains(*id)).cloned());
    }
    Ok(lost)
}

/// After a restore: how many admitted runs have no native workflow in the restored
/// database. They are marked and reported when the service starts; nothing is replayed.
pub(crate) async fn restored_report(
    paths: &crate::settings::Paths,
    runtime: &crate::dbos::RuntimeConfig,
) -> Result<Vec<String>> {
    let store = crate::dbos::admission::AdmissionStore::read_only(paths, &runtime.home_id)?;
    let client = runtime.connect_client().await?;
    let result = missing_natives(&client, store.submitted_ids()?).await;
    client.close().await;
    result
}

#[cfg(test)]
mod tests {
    use crate::{
        core,
        settings::{Paths, Settings},
    };
    use serde_json::json;

    #[test]
    fn a_lost_run_settles_once_with_explicit_evidence() {
        let home = tempfile::tempdir().unwrap();
        core::init(home.path()).unwrap();
        let policy = core::load_policy(home.path()).unwrap();
        let config = Settings::for_tests(&policy)
            .runtime_config(&policy)
            .unwrap();
        let store = std::sync::Arc::new(
            crate::dbos::admission::AdmissionStore::initialize(
                &Paths::defaults(home.path()),
                &policy.home_id,
            )
            .unwrap(),
        );
        let core = core::Core::new(
            Paths::defaults(home.path()),
            policy,
            config,
            store,
            Default::default(),
        )
        .unwrap();
        assert!(core.lost_run("r1").unwrap().is_none());
        core.store
            .write(|c| {
                Ok(c.execute(
                    "INSERT INTO forge_lost_runs(run_id,detected) VALUES('r1',1.0)",
                    [],
                )?)
            })
            .unwrap();
        assert!(core.lost_run("r1").unwrap().unwrap().decision.is_none());
        core.reconcile_lost(
            "operator",
            "r1",
            "not_applied",
            "provider shows nothing",
            &json!(null),
        )
        .unwrap();
        assert_eq!(
            core.lost_run("r1").unwrap().unwrap().decision.as_deref(),
            Some("not_applied")
        );
        let again = core.reconcile_lost("operator", "r1", "completed", "x", &json!(null));
        assert!(
            again
                .unwrap_err()
                .to_string()
                .contains("already reconciled")
        );
    }
}

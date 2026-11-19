//! Operator read models over canonical state: the overview (health, upcoming work,
//! recent failures), agent runs (harness/ACP steps by runner) and schedule run stats.
//! They add no state and no authority: every answer is scoped to the calling actor.
use crate::core::Core;
use anyhow::Result;
use chrono::Utc;
use serde_json::{Value, json};
use std::collections::BTreeMap;

const ACTIVE: [&str; 3] = ["PENDING", "ENQUEUED", "DELAYED"];
const AGENT_SCAN: i64 = 200;
const AGENT_LIMIT: usize = 50;
const STATS_WINDOW: i64 = 100;
const FAILURE_STATUSES: [&str; 3] = [
    "ERROR",
    "RECOVERY_REQUIRED",
    "MAX_RECOVERY_ATTEMPTS_EXCEEDED",
];

impl Core {
    /// Health, upcoming scheduled work and recent failures in one read.
    pub async fn overview(&self, actor: &str) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let database = match self.native() {
            Ok(client) => match client
                .list_workflows(&dbos::sysdb::types::WorkflowFilter {
                    limit: Some(1),
                    load_input: false,
                    load_output: false,
                    ..Default::default()
                })
                .await
            {
                Ok(_) => "ok",
                Err(_) => "unavailable",
            },
            Err(_) => "unavailable",
        };
        let runners = self.runners(actor).await?;
        let runner_rows: Vec<Value> = runners["profiles"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| json!({"name":p["name"],"adapter":p["adapter"],"model":p["model"],"available":p["available"],"version":p["version"],"reason":p["reason"]}))
            .collect();
        let plugins: Vec<Value> = self
            .plugins_view()
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| json!({"id":p["id"],"version":p["version"],"state":p["state"],"enabled":p["enabled"],"problem":p["problem"]}))
            .collect();
        let mut upcoming: Vec<Value> = self
            .schedules(actor)
            .await?
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["status"] == "ACTIVE" && !s["next_run"].is_null())
            .map(|s| json!({"schedule_name":s["schedule_name"],"work_id":s["context"]["work_id"],"next_run":s["next_run"],"schedule":s["schedule"],"cron_timezone":s["cron_timezone"]}))
            .collect();
        upcoming.sort_by(|a, b| {
            a["next_run"]
                .as_str()
                .unwrap_or("")
                .cmp(b["next_run"].as_str().unwrap_or(""))
        });
        upcoming.truncate(10);
        let mut failures = Vec::new();
        for status in FAILURE_STATUSES {
            for run in self
                .search_runs(actor, None, Some(status), Some(10), None)
                .await?["runs"]
                .as_array()
                .into_iter()
                .flatten()
            {
                failures.push(json!({"run_id":run["run_id"],"work_id":run["work_id"],"status":run["status"],"created":run["created"],"failure":run["failure"],"diagnostic":run["diagnostic"]}));
            }
        }
        failures.sort_by(|a, b| {
            b["created"]
                .as_f64()
                .partial_cmp(&a["created"].as_f64())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        failures.truncate(10);
        let approvals = self.approvals(actor, Some("pending"), Some(100)).await?;
        let work = self.store.write(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM forge_work WHERE owner=?1",
                [actor],
                |r| r.get::<_, i64>(0),
            )?)
        })?;
        Ok(json!({
            "health": {
                "service": "ok",
                "engine": "DBOS Rust 0.6.0",
                "database": database,
                "scheduler": "running",
                "time": Utc::now().to_rfc3339(),
                "runners": runner_rows,
                "plugins": plugins,
            },
            "counts": {"work": work, "pending_approvals": approvals["pending"], "failures": failures.len()},
            "upcoming": upcoming,
            "failures": failures,
        }))
    }

    /// Harness and ACP runs by runner profile: active first, then recent. A row is one
    /// harness step of one run; each links to its run.
    pub async fn agents(&self, actor: &str, limit: Option<i64>) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let limit = limit
            .unwrap_or(AGENT_LIMIT as i64)
            .clamp(1, AGENT_LIMIT as i64) as usize;
        let rows = self.store.write(|c| {
            let mut q = c.prepare("SELECT run_id,slot,state,action FROM forge_flow_effects WHERE json_extract(action,'$.type')='harness' ORDER BY rowid DESC LIMIT ?1")?;
            Ok(q.query_map([AGENT_SCAN], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        let mut mine: Vec<(String, String, String, String)> = Vec::new();
        let mut works: BTreeMap<String, String> = BTreeMap::new();
        for row in rows {
            if let Some(owned) = self
                .store
                .inspect(&row.0)?
                .filter(|r| r.envelope.invocation.principal() == actor)
            {
                works.insert(
                    row.0.clone(),
                    owned.envelope.invocation.source_value()["workspace"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned(),
                );
                mine.push(row);
                if mine.len() == limit {
                    break;
                }
            }
        }
        let mut status = BTreeMap::new();
        if let (false, Some(client)) = (mine.is_empty(), self.client.get()) {
            let mut ids: Vec<&str> = mine.iter().map(|r| r.0.as_str()).collect();
            ids.sort_unstable();
            ids.dedup();
            for r in client
                .list_workflows(&dbos::sysdb::types::WorkflowFilter {
                    workflow_ids: ids,
                    load_input: false,
                    load_output: false,
                    ..Default::default()
                })
                .await?
            {
                status.insert(r.workflow_id.clone(), r.status.as_str().to_owned());
            }
        }
        let now = Utc::now().timestamp_millis();
        let (mut active, mut recent) = (Vec::new(), Vec::new());
        for (run_id, slot, effect_state, action) in mine {
            let action: Value = serde_json::from_str(&action).unwrap_or(Value::Null);
            let profile = action["input"]["profile"].as_str().unwrap_or("").to_owned();
            let runner = self.policy.runners.get(&profile);
            let invocation = Self::harness_invocation(&run_id, &slot, 1);
            let dir = self.harness_dir(&invocation);
            let attempt: Value = std::fs::read(dir.join("attempt.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or(Value::Null);
            let captured: Value = std::fs::read(dir.join("captured.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or(Value::Null);
            let native = status.get(&run_id).cloned();
            let is_active = native.as_deref().is_some_and(|s| ACTIVE.contains(&s));
            let shown = if effect_state == "uncertain" && !is_active && native.is_some() {
                Some("RECOVERY_REQUIRED".to_owned())
            } else {
                native.clone()
            };
            let started = attempt["started_ms"].as_i64();
            let finished = attempt["finished_ms"].as_i64();
            let duration = started.map(|s| finished.unwrap_or(now) - s);
            let entry = json!({
                "run_id": run_id,
                "work_id": works.get(&run_id),
                "step": slot,
                "runner": profile,
                "adapter": runner.map(|r| r.adapter.clone()),
                "transport": attempt["transport"],
                "model": captured["served_model"].as_str().map(str::to_owned).or_else(|| runner.and_then(|r| r.model.clone())),
                "status": shown,
                "effect_state": effect_state,
                "attempt_state": attempt["state"],
                "started_ms": started,
                "duration_ms": duration,
                "tool_events": attempt["tool_events"],
                "permission_denials": attempt["permission_decisions"].as_array().map(|d| d.iter().filter(|x| x["decision"] != "allowed_once").count()),
                "actions": {"cancel": is_active},
            });
            if is_active {
                active.push(entry);
            } else {
                recent.push(entry);
            }
        }
        Ok(json!({"active":active,"recent":recent}))
    }

    /// Per-schedule run statistics over each schedule's most recent occurrences.
    pub async fn schedule_stats(&self, actor: &str) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let mut out = Vec::new();
        for s in self
            .schedules(actor)
            .await?
            .as_array()
            .into_iter()
            .flatten()
        {
            let name = s["schedule_name"].as_str().unwrap_or("");
            let history = self
                .schedule_occurrences(actor, name, Some(STATS_WINDOW))
                .await?;
            let mut counts: BTreeMap<String, u64> = BTreeMap::new();
            let occurrences = history["occurrences"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for o in &occurrences {
                *counts
                    .entry(o["outcome"].as_str().unwrap_or("unknown").to_owned())
                    .or_default() += 1;
            }
            let last = occurrences.first();
            out.push(json!({
                "schedule_name": name,
                "work_id": s["context"]["work_id"],
                "status": s["status"],
                "schedule": s["schedule"],
                "cron_timezone": s["cron_timezone"],
                "last_fired_at": s["last_fired_at"],
                "next_run": s["next_run"],
                "window": occurrences.len(),
                "window_limit": STATS_WINDOW,
                "succeeded": counts.get("succeeded").copied().unwrap_or(0),
                "failed": counts.get("failed").copied().unwrap_or(0) + counts.get("not_admitted").copied().unwrap_or(0),
                "cancelled": counts.get("cancelled").copied().unwrap_or(0),
                "running": counts.get("running").copied().unwrap_or(0) + counts.get("handoff_pending").copied().unwrap_or(0),
                "last_outcome": last.map(|o| o["outcome"].clone()),
                "last_run_id": last.map(|o| o["run_id"].clone()),
            }));
        }
        Ok(json!({"schedules":out}))
    }
}

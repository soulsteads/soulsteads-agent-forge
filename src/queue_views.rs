//! Operator view of the home's native DBOS queues. Read-only.
//!
//! DBOS owns every queue: their stored concurrency policy, which workflow is
//! dequeued next and when. Forge reads that state through the supported SDK
//! (`Client::queue`, bounded `list_workflows` filtered by queue and status, and
//! `list_workflow_steps` for the caller's own running runs). No private SQL
//! schema and no second accounting store are used.
//!
//! Counts are home-local: they cover every workflow on the queue in this home,
//! including other actors' work, so capacity can be explained truthfully.
//! Identities, parameters, steps and results are listed only for runs and
//! schedule handoffs the caller owns. A queue position or start estimate is
//! never reported: DBOS does not expose one, and creation order is not a
//! promise of dequeue order.
use crate::core::{Core, Invalid, SCHEDULED, ScheduleContext, WORKFLOW};
use ::dbos::sysdb::types::{Timestamp, WorkflowFilter, WorkflowRecord, WorkflowStatus};
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Most nonterminal native workflows read per queue. Beyond this the counts are
/// labelled as lower bounds (`complete: false`), never extrapolated.
pub const MEMBER_SCAN_MAX: i64 = 500;
pub const MEMBER_LIMIT_DEFAULT: i64 = 50;
pub const MEMBER_LIMIT_MAX: i64 = 100;
/// Recently created, finished native workflows examined for the caller's own
/// recent results, and how many of those are shown.
const RECENT_SCAN: i64 = 100;
const RECENT_SHOWN: usize = 10;
pub const QUEUE_KEYS: &[&str] = &["work", "models", "internal"];

const POSITION_NOTE: &str = "Not reported: DBOS does not expose a queue position or start estimate, and creation order is not a promise of dequeue order.";
const WAITING_CANCEL_NOTE: &str = "Cancelling while it is still waiting means DBOS never starts it, so no step runs. If DBOS dequeued it in the meantime, cancellation is cooperative like any running run.";
const RUNNING_CANCEL_NOTE: &str = "Cancellation is cooperative: it stops before the next checkpoint and cannot undo an external operation already in progress.";

fn ms(t: Option<Timestamp>) -> Value {
    t.map(|t| json!(t.as_epoch_ms())).unwrap_or(Value::Null)
}
fn state_of(status: WorkflowStatus) -> &'static str {
    match status {
        WorkflowStatus::Pending => "running",
        WorkflowStatus::Enqueued => "waiting",
        WorkflowStatus::Delayed => "delayed",
        _ => "finished",
    }
}
/// Native evidence for one owned member, exactly as DBOS records it.
fn native_evidence(row: &WorkflowRecord) -> Value {
    json!({
        "workflow_id": row.workflow_id,
        "status": row.status.as_str(),
        "name": row.name,
        "queue_name": row.queue_name,
        "created_at": row.created_at.as_epoch_ms(),
        "started_at": ms(row.started_at),
        "completed_at": ms(row.completed_at),
        "executor_id": row.executor_id,
        "app_version": row.app_version,
        "recovery_attempts": row.recovery_attempts,
        "priority": row.priority,
        "queue_partition_key": row.queue_partition_key,
        "deduplication_id": row.deduplication_id,
        "rate_limited": row.rate_limited,
        "delay_until": ms(row.delay_until),
        "schedule_name": row.schedule_name,
    })
}

/// Who owns a native queue member, as far as the caller may know.
enum Owner {
    Run { work_id: String },
    Handoff { schedule: String, work_id: String },
    Other,
    Internal,
}

/// One queue's stored native policy. `None` when DBOS has no stored record.
fn policy(queue: Option<&::dbos::Queue>) -> Value {
    let Some(q) = queue else {
        return Value::Null;
    };
    let rate = |r: Option<::dbos::sysdb::types::RateLimit>| {
        r.map(|r| json!({"limit": r.limit, "period_ms": r.period.as_millis() as u64}))
            .unwrap_or(Value::Null)
    };
    json!({
        "concurrency": q.concurrency(),
        "worker_concurrency": q.worker_concurrency(),
        "rate_limit": rate(q.rate_limit()),
        "partitioned": q.is_partitioned(),
        "partition_concurrency": q.partition_concurrency(),
        "partition_worker_concurrency": q.partition_worker_concurrency(),
        "partition_rate_limit": rate(q.partition_rate_limit()),
        "polling_interval_ms": q.polling_interval().as_millis() as u64,
    })
}

impl Core {
    fn owner_of(&self, actor: &str, key: &str, row: &WorkflowRecord) -> Result<Owner> {
        if key == "internal" {
            return Ok(Owner::Internal);
        }
        match row.name.as_deref() {
            Some(WORKFLOW) => {
                let owned = self
                    .store
                    .inspect(&row.workflow_id)?
                    .filter(|r| r.envelope.invocation.principal() == actor);
                Ok(owned
                    .map(|r| Owner::Run {
                        work_id: r.envelope.invocation.workspace().to_owned(),
                    })
                    .unwrap_or(Owner::Other))
            }
            Some(SCHEDULED) => {
                // Ownership comes from the retained schedule definition for the
                // exact native schedule name; names are never split or suffix-matched.
                let Some(name) = row.schedule_name.as_deref() else {
                    return Ok(Owner::Other);
                };
                let retained = self.store.write(|c| {
                    Ok(c.query_row(
                        "SELECT context FROM forge_schedules WHERE name=?1",
                        [name],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?)
                })?;
                let context =
                    retained.and_then(|c| serde_json::from_str::<ScheduleContext>(&c).ok());
                Ok(match context {
                    Some(c) if c.actor == actor && c.name == name => Owner::Handoff {
                        schedule: name.to_owned(),
                        work_id: c.work_id,
                    },
                    _ => Owner::Other,
                })
            }
            _ => Ok(Owner::Other),
        }
    }

    /// What the caller's running member is doing, from its own native checkpoints.
    async fn activity(&self, id: &str, now: i64) -> Result<Value> {
        let steps = self.native()?.list_workflow_steps(id).await?;
        let Some(last) = steps.iter().max_by_key(|s| s.step_id) else {
            return Ok(
                json!({"kind": "started", "explanation": "Dequeued; no step checkpoint is recorded yet."}),
            );
        };
        let wake = last.completed_at.map(|t| t.as_epoch_ms());
        if last.step_name == ::dbos::sysdb::types::step_names::SLEEP
            && last.error.is_none()
            && wake.is_some_and(|w| w > now)
        {
            return Ok(json!({
                "kind": "durable_sleep",
                "step_id": last.step_id,
                "since": ms(last.started_at),
                "until": wake,
                "explanation": "In a native DBOS durable sleep. It still holds its work-queue slot until it wakes and finishes.",
            }));
        }
        Ok(json!({
            "kind": "executing",
            "checkpoints": steps.len(),
            "last_checkpoint": {"step_id": last.step_id, "name": last.step_name, "completed_at": ms(last.completed_at), "error": last.error.is_some()},
            "explanation": "Executing; the last recorded checkpoint is shown. The step after it may be in progress.",
        }))
    }

    /// The canonical queue projection used by the dashboard, CLI and MCP.
    pub async fn queues(
        &self,
        actor: &str,
        only: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Value> {
        self.permit(actor, "read", None)?;
        if let Some(key) = only
            && !QUEUE_KEYS.contains(&key)
        {
            return Err(Invalid("queue must be one of work, models, internal").into());
        }
        let limit = limit.unwrap_or(MEMBER_LIMIT_DEFAULT);
        if !(1..=MEMBER_LIMIT_MAX).contains(&limit) {
            return Err(Invalid("limit must be 1-100").into());
        }
        let native = self.native()?;
        let now = Timestamp::now().as_epoch_ms();
        let names = [
            ("work", self.config.work_queue()),
            ("models", self.config.model_queue()),
            ("internal", ::dbos::sysdb::INTERNAL_QUEUE.to_owned()),
        ];
        let mut queues = Vec::new();
        let mut on_native = BTreeSet::new();
        for (key, name) in names {
            if only.is_some_and(|k| k != key) {
                continue;
            }
            let stored = native.queue(&name).await?;
            let rows = native
                .list_workflows(&WorkflowFilter {
                    queue_names: vec![&name],
                    status: vec![
                        WorkflowStatus::Pending,
                        WorkflowStatus::Enqueued,
                        WorkflowStatus::Delayed,
                    ],
                    limit: Some(MEMBER_SCAN_MAX + 1),
                    load_input: false,
                    load_output: false,
                    ..Default::default()
                })
                .await?;
            let complete = rows.len() as i64 <= MEMBER_SCAN_MAX;
            let rows = &rows[..rows.len().min(MEMBER_SCAN_MAX as usize)];
            let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
            let mut outside: BTreeMap<&str, i64> = BTreeMap::new();
            let mut owned = Vec::new();
            for row in rows {
                let state = state_of(row.status);
                *counts.entry(state).or_default() += 1;
                on_native.insert(row.workflow_id.clone());
                match self.owner_of(actor, key, row)? {
                    Owner::Run { work_id } => owned.push((row, "run", work_id, None)),
                    Owner::Handoff { schedule, work_id } => {
                        owned.push((row, "schedule_handoff", work_id, Some(schedule)))
                    }
                    Owner::Other | Owner::Internal => *outside.entry(state).or_default() += 1,
                }
            }
            let running = counts.get("running").copied().unwrap_or(0);
            let slots = stored.as_ref().and_then(|q| q.concurrency());
            let (free, saturated) = capacity_facts(slots, running, complete);
            let mut found: BTreeMap<&str, i64> = BTreeMap::new();
            for (row, ..) in &owned {
                *found.entry(state_of(row.status)).or_default() += 1;
            }
            let mut shown: BTreeMap<&str, i64> = BTreeMap::new();
            let mut members = Vec::new();
            let mut slot_holders = Vec::new();
            for (row, kind, work_id, schedule) in owned.iter().take(limit as usize) {
                let state = state_of(row.status);
                *shown.entry(state).or_default() += 1;
                let mut member = json!({
                    "kind": kind,
                    "state": state,
                    "work_id": work_id,
                    "created_at": row.created_at.as_epoch_ms(),
                    "started_at": ms(row.started_at),
                    "native": native_evidence(row),
                });
                if *kind == "run" {
                    member["run_id"] = json!(row.workflow_id);
                    if let Ok(e) = self.owned(actor, &row.workflow_id) {
                        member["params"] = self
                            .invocation_params(&e)
                            .map(|p| crate::inspection::preview(&p))
                            .unwrap_or(Value::Null);
                    }
                    member["actions"] = json!({"inspect": true, "cancel": true});
                    member["cancel_note"] = json!(if state == "running" {
                        RUNNING_CANCEL_NOTE
                    } else {
                        WAITING_CANCEL_NOTE
                    });
                } else {
                    member["schedule_name"] = json!(schedule);
                    member["handoff_id"] = json!(row.workflow_id);
                    member["actions"] = json!({"inspect": false, "cancel": false});
                    member["note"] = json!(
                        "A due schedule tick's admission handoff. It admits the run and finishes without waiting for it; follow the schedule's history for the admitted run."
                    );
                }
                match state {
                    "running" => {
                        if *kind == "run" {
                            member["activity"] = self.activity(&row.workflow_id, now).await?;
                        }
                        slot_holders.push(json!({"kind": kind, "id": row.workflow_id, "activity": member["activity"]["kind"]}));
                    }
                    _ => member["wait"] = self.why_waiting(row, slots, running, saturated, now),
                }
                members.push(member);
            }
            let role = match key {
                "work" => {
                    "Forge runs (manual, retried and scheduled) and the admission handoffs of due schedule ticks. Global execution slots are shared by every actor in this home."
                }
                "models" => {
                    "Reserved for model-provider dispatch with per-provider partitions. No Forge workflow is submitted to it in this release: model calls run as checkpointed steps inside work-queue runs, so its provider partition limit does not delay any run today."
                }
                _ => {
                    "DBOS-owned internal queue (Forge's submission maintenance schedule fires here). Not operator work: identities are not listed and nothing here can be cancelled."
                }
            };
            let expected = match key {
                "work" => json!({"concurrency": self.config.work_concurrency}),
                "models" => {
                    json!({"concurrency": self.config.model_concurrency, "partition_concurrency": self.config.server_concurrency})
                }
                _ => Value::Null,
            };
            let summary =
                queue_summary(key, slots, running, &counts, &outside, saturated, complete);
            let (recent, recent_window) = if key == "work" {
                self.recent_results(actor, &name).await?
            } else {
                (json!([]), Value::Null)
            };
            queues.push(json!({
                "key": key,
                "name": name,
                "role": role,
                "policy": policy(stored.as_ref()),
                "home_policy": expected,
                "capacity": {
                    "slots": slots,
                    "running": running,
                    "free": free,
                    "saturated": saturated,
                    "known": saturated.is_some(),
                    "kind": if key == "models" { "global slots, plus one running workflow per provider partition" } else { "global slots" },
                },
                "counts": {
                    "running": running,
                    "waiting": counts.get("waiting").copied().unwrap_or(0),
                    "delayed": counts.get("delayed").copied().unwrap_or(0),
                    "outside_your_scope": {
                        "running": outside.get("running").copied().unwrap_or(0),
                        "waiting": outside.get("waiting").copied().unwrap_or(0),
                        "delayed": outside.get("delayed").copied().unwrap_or(0),
                    },
                    "complete": complete,
                    "scan_limit": MEMBER_SCAN_MAX,
                },
                "summary": summary,
                "slot_holders_yours": if key == "internal" { json!([]) } else { json!(slot_holders) },
                "members": members,
                "members_shown": members.len(),
                "members_yours": owned.len(),
                "members_listing": member_listing(key, complete, &found, &shown, limit),
                "recent": recent,
                "recent_window": recent_window,
            }));
        }
        let pending_handoff = if only.is_none_or(|k| k == "work") {
            self.unsubmitted(actor, &on_native)?
        } else {
            json!({"yours": [], "home_total": null})
        };
        Ok(json!({
            "observed_at": now,
            "queues": queues,
            "admitted_not_on_queue": pending_handoff,
            "observation": {
                "source": "Native DBOS records read through the supported SDK: stored queue policy, nonterminal workflows per queue (bounded), and step checkpoints of your own running runs.",
                "counts": "Home-local totals for this home's DBOS schema, including work outside your scope (counted, never identified). If complete is false, counts are lower bounds from the oldest scan_limit records: free slots and saturation are then unknown (null) unless the observed running count already reaches the limit.",
                "members": "members_listing states whether your listed running/waiting work is complete. If the native scan was incomplete or the member limit omitted some of yours, an empty or short list does not mean you have no work there; use Runs to find it.",
                "identities": "Listed only for runs and schedule handoffs you own.",
                "position": POSITION_NOTE,
                "recent": "Recent results are a sample: of the newest recent_window.scan_limit finished workflows on the work queue (all actors), up to shown_limit of yours, with measured wait (created to dequeued) and run time. recent_window.complete says whether that sample can contain all of yours; older results are in Runs. DBOS clears the queue of a cancelled workflow, so cancelled runs are listed in Runs instead.",
                "ownership": "DBOS owns queue policy, admission order and execution. Forge does not reorder, prioritise, rate-limit or reconfigure queues from this view; capacity is the home's fixed policy.",
            },
        }))
    }

    /// Why one owned queued member has not started, from known evidence only.
    fn why_waiting(
        &self,
        row: &WorkflowRecord,
        slots: Option<i32>,
        running: i64,
        saturated: Option<bool>,
        now: i64,
    ) -> Value {
        let delay = row.delay_until.map(|t| t.as_epoch_ms());
        let (reason, explanation) = if row.status == WorkflowStatus::Delayed
            || delay.is_some_and(|d| d > now)
        {
            (
                "native_delay",
                format!(
                    "Held by a native DBOS delay until {}; DBOS makes it eligible after that instant.",
                    delay
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| "an unrecorded instant".into())
                ),
            )
        } else if row.rate_limited {
            (
                "rate_limited",
                "DBOS marked it as held back by the queue's rate limiter.".to_owned(),
            )
        } else if saturated == Some(true) {
            (
                "queue_capacity",
                format!(
                    "Every work slot is occupied: {running} running of {} allowed. It becomes eligible when a running workflow finishes or is cancelled.",
                    slots.unwrap_or_default()
                ),
            )
        } else if saturated.is_none() {
            (
                "not_determined",
                format!(
                    "Whether a slot is free is unknown: the native scan was incomplete and observed at least {running} running of {} allowed. The native records read do not state why this workflow has not been claimed.",
                    slots.unwrap_or_default()
                ),
            )
        } else {
            (
                "not_determined",
                format!(
                    "A slot was free when observed ({running} running of {}). DBOS dequeues on its next poll; the native records do not state why this workflow has not been claimed yet.",
                    slots
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "unlimited".into())
                ),
            )
        };
        json!({
            "reason": reason,
            "explanation": explanation,
            "waiting_ms": now - row.created_at.as_epoch_ms(),
            "position": Value::Null,
            "position_note": POSITION_NOTE,
        })
    }

    /// The caller's most recently created runs from the work queue that finished,
    /// with measured wait (created → dequeued) and run time (dequeued → finished).
    async fn recent_results(&self, actor: &str, name: &str) -> Result<(Value, Value)> {
        let rows = self
            .native()?
            .list_workflows(&WorkflowFilter {
                queue_names: vec![name],
                names: vec![WORKFLOW],
                status: vec![
                    WorkflowStatus::Success,
                    WorkflowStatus::Error,
                    WorkflowStatus::Cancelled,
                    WorkflowStatus::MaxRecoveryAttemptsExceeded,
                ],
                limit: Some(RECENT_SCAN),
                sort_desc: true,
                load_input: false,
                load_output: false,
                ..Default::default()
            })
            .await?;
        let examined_all = (rows.len() as i64) < RECENT_SCAN;
        let mut examined = 0usize;
        let mut unavailable = 0usize;
        let mut out = Vec::new();
        for row in &rows {
            examined += 1;
            let owned = self
                .store
                .inspect(&row.workflow_id)?
                .is_some_and(|r| r.envelope.invocation.principal() == actor);
            if !owned {
                continue;
            }
            // An owned finished run whose result cannot be read now is counted as
            // unavailable, never treated as absent; no identity is added for it.
            let Ok(run) = self.inspect(actor, &row.workflow_id).await else {
                unavailable += 1;
                continue;
            };
            let created = row.created_at.as_epoch_ms();
            let started = row.started_at.map(|t| t.as_epoch_ms());
            let completed = row.completed_at.map(|t| t.as_epoch_ms());
            let mut summary = crate::inspection::run_summary(run);
            summary["waited_ms"] = json!(started.map(|s| s - created));
            summary["ran_ms"] = json!(started.zip(completed).map(|(s, c)| c - s));
            summary["started_at"] = json!(started);
            summary["completed_at"] = json!(completed);
            out.push(summary);
            if out.len() == RECENT_SHOWN {
                break;
            }
        }
        let window = recent_window(rows.len(), examined_all, examined, out.len(), unavailable);
        Ok((json!(out), window))
    }

    /// Owned runs Forge admitted whose handoff to the DBOS work queue is not yet
    /// confirmed. This is never queue capacity.
    fn unsubmitted(&self, actor: &str, on_native: &BTreeSet<String>) -> Result<Value> {
        let (rows, total) = self.store.write(|c| {
            let mut s = c.prepare(
                "SELECT dbos_id,state FROM dbos_invocations WHERE state IN ('admitted','submission_unknown') AND json_extract(envelope,'$.invocation.principal')=?1 ORDER BY rowid LIMIT 100",
            )?;
            let rows = s
                .query_map([actor], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let total: i64 = c.query_row(
                "SELECT count(*) FROM dbos_invocations WHERE state IN ('admitted','submission_unknown')",
                [],
                |r| r.get(0),
            )?;
            Ok((rows, total))
        })?;
        let yours = rows
            .into_iter()
            .filter(|(id, _)| !on_native.contains(id))
            .filter_map(|(id, state)| {
                let record = self.store.inspect(&id).ok()??;
                Some(json!({
                    "run_id": id,
                    "work_id": record.envelope.invocation.workspace(),
                    "admission_state": state,
                    "explanation": "Admitted by Forge; the handoff to the DBOS work queue is not confirmed yet. The submission maintenance pass retries it every 10 seconds. This is not queue capacity.",
                }))
            })
            .collect::<Vec<_>>();
        Ok(json!({"yours": yours, "home_total": total}))
    }
}

/// Free slots and saturation from an observed running count. A complete scan
/// proves both; an incomplete scan proves only saturation (observed running already
/// at the limit). Otherwise both are unknown (`None`), never assumed free.
fn capacity_facts(slots: Option<i32>, running: i64, complete: bool) -> (Option<i64>, Option<bool>) {
    match slots {
        None => (None, Some(false)),
        Some(s) if running >= s as i64 => (Some(0), Some(true)),
        Some(s) if complete => (Some(s as i64 - running), Some(false)),
        Some(_) => (None, None),
    }
}

/// Whether the caller's listed members are complete, overall and per state. Absence
/// is asserted only when the native scan was complete and the member limit omitted
/// nothing in that state.
fn member_listing(
    key: &str,
    scan_complete: bool,
    found: &BTreeMap<&str, i64>,
    shown: &BTreeMap<&str, i64>,
    limit: i64,
) -> Value {
    if key == "internal" {
        return json!({"identities_listed": false, "complete": Value::Null, "note": "Internal identities are never listed; counts only."});
    }
    let get = |m: &BTreeMap<&str, i64>, k| m.get(k).copied().unwrap_or(0);
    let total_found: i64 = found.values().sum();
    let total_shown: i64 = shown.values().sum();
    let omitted = total_found - total_shown;
    let by_state = ["running", "waiting", "delayed"]
        .iter()
        .map(|st| {
            let (f, sh) = (get(found, st), get(shown, st));
            (
                st.to_string(),
                json!({"found": f, "shown": sh, "complete": scan_complete && f == sh}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let mut notes = Vec::new();
    if !scan_complete {
        notes.push(format!("The native scan read only the oldest {MEMBER_SCAN_MAX} nonterminal workflows on this queue, so work of yours created later is not visible here: an empty or short list does not mean you have none. Find it in Runs (agent-forge runs search --status ENQUEUED, or PENDING)."));
    }
    if omitted > 0 {
        notes.push(format!(
            "{omitted} more of your workflows were found but not listed (member limit {limit})."
        ));
    }
    json!({
        "identities_listed": true,
        "native_scan_complete": scan_complete,
        "member_limit": limit,
        "found_in_scan": total_found,
        "shown": total_shown,
        "omitted_by_limit": omitted,
        "complete": scan_complete && omitted == 0,
        "by_state": by_state,
        "note": if notes.is_empty() { Value::Null } else { json!(notes.join(" ")) },
    })
}

/// Honest bounds of the recent-results sample.
fn recent_window(
    read: usize,
    examined_all: bool,
    examined: usize,
    shown: usize,
    unavailable: usize,
) -> Value {
    let stopped = shown == RECENT_SHOWN && examined < read;
    let reached_shown_limit = shown == RECENT_SHOWN;
    let complete = examined_all && !reached_shown_limit && unavailable == 0;
    let note = if complete {
        "Every finished workflow still recorded on this queue was examined.".to_owned()
    } else {
        let mut parts = Vec::new();
        if unavailable > 0 {
            parts.push(format!("{unavailable} of your finished runs in this window could not be read for display right now; they are recorded, not absent. Open them from Runs."));
        }
        if !examined_all || reached_shown_limit {
            parts.push(format!("Sampled: only the newest {RECENT_SCAN} finished workflows on this queue (all actors) are examined and at most {RECENT_SHOWN} of yours shown, so older results of yours may exist."));
        }
        parts.push("See Runs (agent-forge runs search) for full history.".to_owned());
        parts.join(" ")
    };
    json!({
        "scan_limit": RECENT_SCAN,
        "read": read,
        "examined": examined,
        "window_covers_all_finished_on_queue": examined_all,
        "shown_limit": RECENT_SHOWN,
        "shown": shown,
        "stopped_at_shown_limit": stopped,
        "unavailable": unavailable,
        "complete": complete,
        "note": note,
    })
}

fn queue_summary(
    key: &str,
    slots: Option<i32>,
    running: i64,
    counts: &BTreeMap<&str, i64>,
    outside: &BTreeMap<&str, i64>,
    saturated: Option<bool>,
    complete: bool,
) -> String {
    let waiting = counts.get("waiting").copied().unwrap_or(0);
    let delayed = counts.get("delayed").copied().unwrap_or(0);
    let other = outside.values().sum::<i64>();
    let at_least = if complete { "" } else { "at least " };
    let slots_text = slots
        .map(|s| format!("{running} of {s} slots running"))
        .unwrap_or_else(|| format!("{running} running (no global limit)"));
    let mut text = format!("{at_least}{slots_text}; {waiting} waiting; {delayed} delayed");
    if other > 0 && key != "internal" {
        text.push_str(&format!(" ({other} outside your scope, counted only)"));
    }
    text.push('.');
    if saturated.is_none() {
        text.push_str(" Free slots unknown: the native scan was incomplete, so the running count is a lower bound below the limit.");
    }
    let saturated = saturated == Some(true);
    if key == "work" {
        if saturated && waiting + delayed > 0 {
            text.push_str(" Saturated: waiting work starts as running work finishes. To start a waiting run sooner, cancel running or waiting runs you no longer need.");
        } else if saturated {
            text.push_str(" Saturated: new work will wait for a free slot.");
        } else if complete && running == 0 && waiting + delayed == 0 {
            text.push_str(" Idle.");
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summary_states_capacity_without_estimates() {
        let mut counts = BTreeMap::new();
        counts.insert("waiting", 3);
        let mut outside = BTreeMap::new();
        outside.insert("running", 1);
        let s = queue_summary("work", Some(2), 2, &counts, &outside, Some(true), true);
        assert!(s.starts_with("2 of 2 slots running; 3 waiting; 0 delayed (1 outside your scope"));
        assert!(s.contains("Saturated"));
        assert!(!s.contains("position") && !s.contains("estimated"));
        let s = queue_summary(
            "work",
            Some(2),
            0,
            &BTreeMap::new(),
            &BTreeMap::new(),
            Some(false),
            true,
        );
        assert!(s.starts_with("0 of 2"));
        assert!(s.ends_with("Idle."));
    }

    // Synthetic boundary diagnostics (Q03-R1/R2): projections of bounded native reads.
    // They are not a live 501-workflow run.

    #[test]
    fn incomplete_scan_below_the_limit_leaves_free_slots_unknown() {
        // 500 older partner rows read; observed 1 running of 2 cannot prove a free slot.
        assert_eq!(capacity_facts(Some(2), 1, false), (None, None));
        // Lower-bound saturation is still definite.
        assert_eq!(capacity_facts(Some(2), 2, false), (Some(0), Some(true)));
        assert_eq!(capacity_facts(Some(2), 1, true), (Some(1), Some(false)));
        assert_eq!(capacity_facts(None, 7, false), (None, Some(false)));
        let mut counts = BTreeMap::new();
        counts.insert("waiting", 499);
        let s = queue_summary("work", Some(2), 1, &counts, &BTreeMap::new(), None, false);
        assert!(s.starts_with("at least 1 of 2 slots running"));
        assert!(
            s.contains("Free slots unknown") && !s.contains("Idle") && !s.contains("Saturated")
        );
        let s = queue_summary(
            "work",
            Some(2),
            0,
            &BTreeMap::new(),
            &BTreeMap::new(),
            None,
            false,
        );
        assert!(!s.contains("Idle"));
    }

    #[test]
    fn older_partner_rows_hiding_owned_work_are_not_reported_as_absence() {
        // 500 older partner workflows fill the scan; the operator's later run is row 501.
        let none = BTreeMap::new();
        let l = member_listing("work", false, &none, &none, 50);
        assert_eq!(l["complete"], false);
        assert_eq!(l["found_in_scan"], 0);
        assert_eq!(l["by_state"]["waiting"]["complete"], false);
        assert_eq!(l["by_state"]["running"]["complete"], false);
        assert!(
            l["note"]
                .as_str()
                .unwrap()
                .contains("does not mean you have none")
        );
        // Complete scan with nothing owned: absence is proven.
        let l = member_listing("work", true, &none, &none, 50);
        assert_eq!(l["complete"], true);
        assert!(l["note"].is_null());
        // Internal identities are withheld, not absent.
        let l = member_listing("internal", true, &none, &none, 50);
        assert_eq!(l["identities_listed"], false);
        assert!(l["complete"].is_null());
    }

    #[test]
    fn member_limit_omitting_a_state_does_not_claim_that_state_is_empty() {
        let mut found = BTreeMap::new();
        found.insert("running", 1);
        found.insert("waiting", 2);
        let mut shown = BTreeMap::new();
        shown.insert("running", 1);
        let l = member_listing("work", true, &found, &shown, 1);
        assert_eq!(l["complete"], false);
        assert_eq!(l["omitted_by_limit"], 2);
        assert_eq!(l["by_state"]["running"]["complete"], true);
        assert_eq!(
            l["by_state"]["waiting"],
            json!({"found": 2, "shown": 0, "complete": false})
        );
        assert!(
            l["note"]
                .as_str()
                .unwrap()
                .contains("2 more of your workflows were found but not listed (member limit 1)")
        );
    }

    #[test]
    fn recent_window_full_of_partner_results_is_labelled_sampled() {
        // Newest 100 finished workflows are all partner runs; the operator's older success is outside.
        let w = recent_window(100, false, 100, 0, 0);
        assert_eq!(w["complete"], false);
        assert_eq!(w["window_covers_all_finished_on_queue"], false);
        assert!(w["note"].as_str().unwrap().starts_with("Sampled"));
        // Fewer than 100 finished in total and fewer than 10 of yours: the sample is complete.
        let w = recent_window(3, true, 3, 1, 0);
        assert_eq!(w["complete"], true);
        // Ten of yours shown: older ones may exist even if the window was not full.
        let w = recent_window(40, true, 12, 10, 0);
        assert_eq!(w["complete"], false);
        assert_eq!(w["stopped_at_shown_limit"], true);
    }

    #[test]
    fn owned_result_that_cannot_be_inspected_is_not_reported_as_absent() {
        // SYNTHETIC: one owned finished row in a short window whose inspection failed.
        let w = recent_window(1, true, 1, 0, 1);
        assert_eq!(w["complete"], false);
        assert_eq!(w["unavailable"], 1);
        let note = w["note"].as_str().unwrap();
        assert!(note.contains("1 of your finished runs in this window could not be read"));
        assert!(note.contains("not absent") && !note.contains("Sampled"));
    }

    #[test]
    fn delayed_member_omitted_by_the_limit_keeps_the_waiting_section_incomplete() {
        // SYNTHETIC: complete scan, older running shown at limit 1, later delayed omitted, no ENQUEUED.
        let mut found = BTreeMap::new();
        found.insert("running", 1);
        found.insert("delayed", 1);
        let mut shown = BTreeMap::new();
        shown.insert("running", 1);
        let l = member_listing("work", true, &found, &shown, 1);
        assert_eq!(l["by_state"]["waiting"]["complete"], true);
        assert_eq!(l["by_state"]["delayed"]["complete"], false);
        assert_eq!(l["complete"], false);
    }
    #[test]
    fn member_states_follow_native_status() {
        assert_eq!(state_of(WorkflowStatus::Pending), "running");
        assert_eq!(state_of(WorkflowStatus::Enqueued), "waiting");
        assert_eq!(state_of(WorkflowStatus::Delayed), "delayed");
        assert_eq!(state_of(WorkflowStatus::Success), "finished");
    }
}

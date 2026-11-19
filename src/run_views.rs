//! Operator projections over canonical run and work state: structured run
//! search and per-step workflow interpretation. Read-only; every record comes
//! from the same scoped `inspect`/`work` paths used by API, CLI and MCP.
use crate::core::{Core, Invalid};
use anyhow::Result;
use serde_json::{Value, json};

pub const SEARCH_LIMIT_DEFAULT: i64 = 25;
pub const SEARCH_LIMIT_MAX: i64 = 100;
/// Bounded scan so a narrow status filter can never walk an unbounded history.
pub const SEARCH_SCAN_MAX: usize = 500;
pub const STATUSES: &[&str] = &[
    "ADMITTED",
    "PENDING",
    "ENQUEUED",
    "DELAYED",
    "SUCCESS",
    "ERROR",
    "CANCELLED",
    "MAX_RECOVERY_ATTEMPTS_EXCEEDED",
    "RECOVERY_REQUIRED",
];
const ACTIVE: &[&str] = &["ADMITTED", "PENDING", "ENQUEUED", "DELAYED"];
const RETRYABLE: &[&str] = &["ERROR", "CANCELLED", "MAX_RECOVERY_ATTEMPTS_EXCEEDED"];

/// Actions the canonical service would currently consider for a run status.
/// The service re-checks authority and effect state on every request.
pub(crate) fn actions(status: &str) -> Value {
    json!({"cancel": ACTIVE.contains(&status), "retry": RETRYABLE.contains(&status), "reconcile": status == "RECOVERY_REQUIRED"})
}

fn step_prefix(id: &str) -> String {
    format!("forge.flow.root/step:{id}")
}
fn belongs(name: &str, id: &str) -> bool {
    let prefix = step_prefix(id);
    name == prefix || name.starts_with(&format!("{prefix}/"))
}

/// Top-level authored step a located refusal belongs to. Refusals are recorded
/// as `root/step:<id>: ...` (pre-dispatch at that step) or
/// `root/step:<id>/step:...` (inside a nested flow at that step).
fn located_step(detail: &str, ids: &[&str]) -> Option<usize> {
    let rest = detail.strip_prefix("root/step:")?;
    let end = rest.find([':', '/'])?;
    ids.iter().position(|id| *id == &rest[..end])
}

const NESTED_OUTPUT_NOTE: &str =
    "Returned; nested workflow output is published only with a successful parent run";
const UNKNOWN_NOTE: &str =
    "Not determinable: native checkpoints and refusal audit do not establish this step's outcome";

/// Interpret authored workflow steps against native step checkpoints, the
/// published output and the run's refusal audit. Never infers success: a step
/// is `completed` only with a recorded output, a later step's start, or a
/// located refusal at a later sequential step; an unexplained stop is reported
/// as such. A nested step's aggregate output is never replaced by a leaf output.
/// `failed`/`not_reached` are asserted only from compatible evidence (an error
/// checkpoint or an accepted located refusal); a recorded checkpoint is never
/// labelled `not_reached`. Missing or contradictory evidence yields `unknown`
/// steps and a failure whose `location` is `unknown`, carrying the unattributed
/// refusal details as unverified evidence instead of a definite reason.
pub(crate) fn workflow_steps(definition: &Value, run: &Value) -> Option<Value> {
    if definition["type"] != "workflow" {
        return None;
    }
    let authored = definition["input"]["steps"].as_array()?;
    let status = run["status"].as_str().unwrap_or("");
    let active = ACTIVE.contains(&status);
    let native = run["steps"].as_array().cloned().unwrap_or_default();
    let refusals: Vec<&str> = run["logs"]
        .as_array()
        .map(|logs| {
            logs.iter()
                .filter(|l| l["action"] == "flow_refused")
                .filter_map(|l| l["detail"].as_str())
                .collect()
        })
        .unwrap_or_default();
    let published = run["output"]["steps"].as_object();
    let ids: Vec<&str> = authored
        .iter()
        .map(|s| s["id"].as_str().unwrap_or(""))
        .collect();
    let checkpoints = |id: &str| -> Vec<&Value> {
        native
            .iter()
            .filter(|s| s["function_name"].as_str().is_some_and(|n| belongs(n, id)))
            .collect()
    };
    // Steps run strictly in authored order, so a refusal located at step L
    // proves steps before L returned. Use it only when the native evidence
    // agrees: no earlier step errored and no later step left a checkpoint.
    let located = refusals
        .iter()
        .rev()
        .find_map(|d| located_step(d, &ids).map(|i| (i, (*d).to_owned())))
        .filter(|(at, _)| {
            ids[..*at]
                .iter()
                .all(|id| checkpoints(id).iter().all(|s| s["error"].is_null()))
                && ids[at + 1..].iter().all(|id| checkpoints(id).is_empty())
        });
    let mut steps = Vec::new();
    let mut failed: Option<Value> = None;
    let mut blocked = false;
    let mut uncertain = false;
    for (index, step) in authored.iter().enumerate() {
        let id = ids[index];
        let action = &step["action"];
        let kind = action["work_id"]
            .as_str()
            .map(|w| format!("work:{w}"))
            .unwrap_or_else(|| action["type"].as_str().unwrap_or("").to_owned());
        let mine = checkpoints(id);
        let started = mine.iter().filter_map(|s| s["started_at"].as_i64()).min();
        let completed_at = mine.iter().filter_map(|s| s["completed_at"].as_i64()).max();
        let errored = mine.iter().any(|s| !s["error"].is_null());
        let exact = mine
            .iter()
            .find(|s| s["function_name"] == step_prefix(id).as_str())
            .filter(|s| !s["output"].is_null());
        let later_started = ids[index + 1..]
            .iter()
            .any(|next| !checkpoints(next).is_empty());
        // Only an accepted located refusal is a definite reason; an error
        // checkpoint without one reports the native error, not another detail.
        let reason = match &located {
            Some((at, detail)) if *at == index => Some(detail.clone()),
            _ => mine.iter().find_map(|s| {
                s["error"].as_str().map(|e| {
                    format!("Native step checkpoint recorded error {e}; no refusal located here")
                })
            }),
        };
        if blocked && !mine.is_empty() {
            // A checkpoint after a definite stop contradicts it: report this
            // step from its own evidence and stop inferring not_reached.
            blocked = false;
            uncertain = true;
        }
        let returned = |note: &str| match exact {
            Some(exact) => ("completed", exact["output"].clone(), None),
            None => ("completed", Value::Null, Some(note.to_owned())),
        };
        let (state, output, note) = if let Some(out) = published.and_then(|p| p.get(id)) {
            ("completed", out["output"].clone(), None)
        } else if blocked {
            (
                if active { "pending" } else { "not_reached" },
                Value::Null,
                None,
            )
        } else if let Some((at, _)) = located.as_ref().filter(|(at, _)| *at >= index) {
            if *at == index {
                ("failed", Value::Null, None)
            } else {
                returned(if mine.is_empty() {
                    "Returned before a later step was refused; no checkpoint output recorded"
                } else {
                    NESTED_OUTPUT_NOTE
                })
            }
        } else if errored {
            ("failed", Value::Null, None)
        } else if let Some(exact) = exact {
            ("completed", exact["output"].clone(), None)
        } else if later_started && !mine.is_empty() {
            returned(NESTED_OUTPUT_NOTE)
        } else if active {
            (
                if mine.is_empty() {
                    "pending"
                } else {
                    "running"
                },
                Value::Null,
                None,
            )
        } else if status == "SUCCESS" {
            ("completed", Value::Null, None)
        } else if status == "CANCELLED" {
            ("cancelled", Value::Null, None)
        } else {
            uncertain = true;
            ("unknown", Value::Null, Some(UNKNOWN_NOTE.to_owned()))
        };
        if matches!(state, "failed" | "cancelled" | "running" | "pending") {
            blocked = true;
        }
        if state == "failed" && failed.is_none() {
            failed = Some(
                json!({"location":"step","step":id,"index":index+1,"reason":reason.clone().unwrap_or_else(|| "Step stopped without a recorded refusal; inspect native step evidence".into())}),
            );
        }
        let mut row = json!({
            "index": index + 1,
            "id": id,
            "needs": step.get("needs").cloned().unwrap_or(json!([])),
            "action": kind,
            "state": state,
            "started_at": started,
            "completed_at": completed_at,
            "native_steps": mine.iter().map(|s| s["function_id"].clone()).collect::<Vec<_>>(),
            "output": crate::inspection::preview(&output),
            "reason": if state == "failed" { reason.map(Value::String).unwrap_or(Value::Null) } else { Value::Null },
        });
        if let Some(note) = note {
            row["note"] = Value::String(note);
        }
        steps.push(row);
    }
    let stopped = status == "ERROR" || status == "MAX_RECOVERY_ATTEMPTS_EXCEEDED";
    // A located refusal the guard rejected is contradictory evidence too.
    let rejected = located.is_none() && refusals.iter().any(|d| located_step(d, &ids).is_some());
    let failure = failed
        .map(|mut f| {
            if uncertain || rejected {
                f["contradicted"] = Value::Bool(true);
            }
            f
        })
        .or_else(|| {
            if !stopped {
                None
            } else if uncertain || rejected || refusals.is_empty() {
                let hint = format!(
                    "Location unknown: native checkpoints and refusal audit do not establish where this run stopped; inspect native checkpoints and audit with agent-forge runs show {} --json",
                    run["run_id"].as_str().unwrap_or("<run>")
                );
                Some(json!({"location":"unknown","step":Value::Null,"index":Value::Null,"reason":hint,"unverified_refusals":refusals}))
            } else {
                // Every step is consistently explained; an unlocated refusal
                // is a workflow-level stop (for example output mapping).
                refusals
                    .last()
                    .map(|d| json!({"location":"workflow","step":Value::Null,"index":Value::Null,"reason":d}))
            }
        });
    Some(json!({"steps":steps,"failure":failure,"uncertain":uncertain || rejected}))
}

impl Core {
    /// Structured, scoped run search over canonical admissions, newest first.
    /// `before` is the opaque cursor returned as `next_before`.
    pub async fn search_runs(
        &self,
        actor: &str,
        work_id: Option<&str>,
        status: Option<&str>,
        limit: Option<i64>,
        before: Option<&str>,
    ) -> Result<Value> {
        self.permit(actor, "read", None)?;
        if let Some(work_id) = work_id {
            crate::core::identifier(work_id)?;
        }
        if let Some(status) = status
            && !STATUSES.contains(&status)
        {
            return Err(Invalid("unknown run status filter").into());
        }
        let limit = limit.unwrap_or(SEARCH_LIMIT_DEFAULT);
        if !(1..=SEARCH_LIMIT_MAX).contains(&limit) {
            return Err(Invalid("limit must be 1-100").into());
        }
        let before = before
            .map(|b| {
                b.parse::<i64>().ok().filter(|v| *v > 0).ok_or(Invalid(
                    "before must be a cursor returned by a previous search",
                ))
            })
            .transpose()?;
        let candidates = self.store.write(|c| {
            let mut s = c.prepare(
                "SELECT rowid,dbos_id FROM dbos_invocations WHERE json_extract(envelope,'$.invocation.principal')=?1 AND (?2 IS NULL OR json_extract(envelope,'$.invocation.workspace')=?2) AND (?3 IS NULL OR rowid<?3) ORDER BY rowid DESC LIMIT ?4",
            )?;
            Ok(s.query_map(
                rusqlite::params![actor, work_id, before, SEARCH_SCAN_MAX as i64],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        let mut runs = Vec::new();
        let mut scanned = 0usize;
        let mut last = None;
        for (rowid, id) in &candidates {
            scanned += 1;
            last = Some(*rowid);
            let Ok(run) = self.inspect(actor, id).await else {
                continue;
            };
            let run_status = run["status"].as_str().unwrap_or("");
            if status.is_some_and(|s| s != run_status) {
                continue;
            }
            let failure = self
                .work(actor, run["work_id"].as_str().unwrap_or(""))
                .ok()
                .and_then(|w| workflow_steps(&w["definition"], &run))
                .and_then(|w| w["failure"].as_object().cloned());
            let mut summary = crate::inspection::run_summary(run.clone());
            summary["type"] = run["definition"]["type"].clone();
            summary["failure"] = failure.map(Value::Object).unwrap_or(Value::Null);
            summary["actions"] = actions(run_status);
            runs.push(summary);
            if runs.len() as i64 == limit {
                break;
            }
        }
        let exhausted = candidates.len() < SEARCH_SCAN_MAX && scanned == candidates.len();
        let next_before = if exhausted && (runs.len() as i64) < limit {
            Value::Null
        } else {
            last.map(|l| json!(l.to_string())).unwrap_or(Value::Null)
        };
        Ok(json!({
            "filters": {"work_id": work_id, "status": status, "limit": limit, "before": before.map(|b| b.to_string())},
            "runs": runs,
            "scanned": scanned,
            "next_before": next_before,
            "statuses": STATUSES,
        }))
    }
    /// Validate a definition exactly as registration would, without storing it.
    pub fn validate_only(&self, actor: &str, definition: &Value) -> Result<Value> {
        self.check_definition(actor, definition)?;
        let steps = (definition["type"] == "workflow").then(|| {
            definition["input"]["steps"]
                .as_array()
                .map(|s| {
                    s.iter()
                        .map(|step| json!({"id":step["id"],"needs":step.get("needs").cloned().unwrap_or(json!([])),"action":step["action"]["work_id"].as_str().map(|w|format!("work:{w}")).unwrap_or_else(||step["action"]["type"].as_str().unwrap_or("").to_owned())}))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        });
        Ok(
            json!({"valid":true,"type":definition["type"],"steps":steps,"params":definition["input"].get("params").cloned().unwrap_or(json!({})),"output":definition["input"].get("output").cloned()}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn def() -> Value {
        json!({"type":"workflow","input":{"steps":[
            {"id":"a","action":{"type":"echo","input":{"text":"x"}}},
            {"id":"b","needs":["a"],"action":{"type":"fail","input":{}}},
            {"id":"c","action":{"type":"echo","input":{"text":"y"}}}]}})
    }
    #[test]
    fn failed_step_is_located_and_later_steps_not_reached() {
        let run = json!({"status":"ERROR","output":null,"steps":[
            {"function_id":1,"function_name":"forge.flow.root/step:a","output":{"text":"x"},"error":null,"started_at":1,"completed_at":2},
            {"function_id":2,"function_name":"forge.flow.root/step:b","output":null,"error":"WorkFault","started_at":3,"completed_at":4}],
            "logs":[{"action":"flow_refused","detail":"root/step:b: explicit flow failure"}]});
        let w = workflow_steps(&def(), &run).unwrap();
        let states: Vec<_> = w["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["state"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(states, ["completed", "failed", "not_reached"]);
        assert_eq!(w["failure"]["step"], "b");
        assert_eq!(w["failure"]["location"], "step");
        assert_eq!(w["uncertain"], false);
        assert_eq!(w["failure"]["reason"], "root/step:b: explicit flow failure");
        assert_eq!(w["steps"][0]["output"]["text"], "x");
    }
    #[test]
    fn refusal_before_dispatch_names_the_step_without_a_native_checkpoint() {
        let run = json!({"status":"ERROR","output":null,"steps":[
            {"function_id":1,"function_name":"forge.flow.root/step:a","output":{"text":"x"},"error":null}],
            "logs":[{"action":"flow_refused","detail":"root/step:b: input reference failed: flow reference target is missing"}]});
        let w = workflow_steps(&def(), &run).unwrap();
        assert_eq!(w["failure"]["step"], "b");
        assert_eq!(w["steps"][2]["state"], "not_reached");
    }
    fn nested() -> Value {
        json!({"type":"workflow","input":{"steps":[
            {"id":"group","action":{"type":"workflow","input":{"steps":[
                {"id":"inner","action":{"type":"echo","input":{"text":"ok"}}},
                {"id":"inner2","action":{"type":"echo","input":{"text":"${/params/gone}"}}}]}}},
            {"id":"use","action":{"type":"echo","input":{"text":"${/params/missing}"}}},
            {"id":"after","action":{"type":"echo","input":{"text":"z"}}}]}})
    }
    fn states(w: &Value) -> Vec<String> {
        w["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["state"].as_str().unwrap().to_owned())
            .collect()
    }
    #[test]
    fn later_pre_dispatch_refusal_after_completed_nested_step_is_located_at_that_step() {
        // Review F1 shape: the nested echo succeeded, then `use` was refused
        // before any `use` checkpoint existed.
        let run = json!({"status":"ERROR","output":null,"steps":[
            {"function_id":1,"function_name":"forge.flow.root/step:group/step:inner","output":{"text":"ok"},"error":null,"started_at":1,"completed_at":2}],
            "logs":[{"action":"flow_refused","detail":"root/step:use: input reference failed: flow reference target is missing"}]});
        let w = workflow_steps(&nested(), &run).unwrap();
        assert_eq!(states(&w), ["completed", "failed", "not_reached"]);
        assert_eq!(w["failure"]["step"], "use");
        assert_eq!(w["failure"]["index"], 2);
        assert_eq!(w["failure"]["location"], "step");
        assert!(w["failure"].get("contradicted").is_none());
        assert_eq!(w["uncertain"], false);
        assert_eq!(
            w["failure"]["reason"],
            "root/step:use: input reference failed: flow reference target is missing"
        );
        // The nested aggregate was never published: no leaf substitution.
        assert!(w["steps"][0]["output"].is_null());
        assert!(w["steps"][0]["note"].as_str().unwrap().contains("nested"));
        assert_eq!(w["steps"][0]["native_steps"], json!([1]));
        assert_eq!(w["steps"][1]["reason"], w["failure"]["reason"]);
        assert!(w["steps"][2]["reason"].is_null());
    }
    #[test]
    fn refusal_inside_nested_step_stays_located_at_the_nested_step() {
        let run = json!({"status":"ERROR","output":null,"steps":[
            {"function_id":1,"function_name":"forge.flow.root/step:group/step:inner","output":{"text":"ok"},"error":null}],
            "logs":[{"action":"flow_refused","detail":"root/step:group/step:inner2: input reference failed: flow reference target is missing"}]});
        let w = workflow_steps(&nested(), &run).unwrap();
        assert_eq!(states(&w), ["failed", "not_reached", "not_reached"]);
        assert_eq!(w["failure"]["step"], "group");
        assert_eq!(w["failure"]["location"], "step");
        assert_eq!(w["uncertain"], false);
        assert!(
            w["failure"]["reason"]
                .as_str()
                .unwrap()
                .starts_with("root/step:group/step:inner2:")
        );
    }
    #[test]
    fn contradicting_or_absent_evidence_does_not_invent_completion() {
        // Review F1 attempt-2 shape: a located refusal at `use` is contradicted
        // by a successful `after` checkpoint. The refusal is not trusted, never
        // becomes a step's failure reason, and the checkpoint is not hidden.
        let run = json!({"run_id":"r1","status":"ERROR","output":null,"steps":[

            {"function_id":0,"function_name":"forge.admit","output":{},"error":null},

            {"function_id":1,"function_name":"forge.flow.root/step:after","output":{"text":"z"},"error":null}],

            "logs":[{"action":"flow_refused","detail":"root/step:use: input reference failed: x"}]});
        let w = workflow_steps(&nested(), &run).unwrap();
        assert_eq!(states(&w), ["unknown", "unknown", "completed"]);
        assert_eq!(w["uncertain"], true);
        let f = &w["failure"];
        assert_eq!(f["location"], "unknown");
        assert!(f["step"].is_null() && f["index"].is_null());
        let reason = f["reason"].as_str().unwrap();
        assert!(reason.starts_with("Location unknown:"));
        assert!(reason.contains("agent-forge runs show r1 --json"));
        assert!(!reason.contains("input reference failed"));
        assert_eq!(
            f["unverified_refusals"],
            json!(["root/step:use: input reference failed: x"])
        );
        for row in w["steps"].as_array().unwrap() {
            assert!(
                row["reason"].is_null(),
                "no step carries the rejected refusal"
            );
        }
        assert!(
            w["steps"][0]["note"]
                .as_str()
                .unwrap()
                .starts_with("Not determinable")
        );
        assert!(w["steps"][0]["output"].is_null() && w["steps"][1]["output"].is_null());
        // The recorded checkpoint keeps its native output and id.
        assert_eq!(w["steps"][2]["output"]["text"], "z");
        assert_eq!(w["steps"][2]["native_steps"], json!([1]));

        // No evidence at all: nothing is located or asserted not_reached.
        let bare = json!({"run_id":"r2","status":"ERROR","output":null,"steps":[],"logs":[]});
        let w = workflow_steps(&nested(), &bare).unwrap();
        assert_eq!(states(&w), ["unknown", "unknown", "unknown"]);
        assert_eq!(w["failure"]["location"], "unknown");
        assert!(w["failure"]["step"].is_null());
        assert_eq!(w["failure"]["unverified_refusals"], json!([]));
    }
    #[test]
    fn checkpoint_after_a_definite_failure_is_never_not_reached() {
        // An error checkpoint at `b` is recorded failure evidence, but a later
        // checkpoint at `c` contradicts the stop: `c` keeps its own evidence and
        // the failure is flagged as contradicted. The refusal at `b` is
        // rejected by the guard, so the reason comes from the native error.
        let run = json!({"status":"ERROR","output":null,"steps":[

            {"function_id":1,"function_name":"forge.flow.root/step:a","output":{"text":"x"},"error":null},

            {"function_id":2,"function_name":"forge.flow.root/step:b","output":null,"error":"WorkFault"},

            {"function_id":3,"function_name":"forge.flow.root/step:c","output":{"text":"y"},"error":null}],

            "logs":[{"action":"flow_refused","detail":"root/step:b: explicit flow failure"}]});
        let w = workflow_steps(&def(), &run).unwrap();
        assert_eq!(states(&w), ["completed", "failed", "completed"]);
        assert_eq!(w["failure"]["location"], "step");
        assert_eq!(w["failure"]["step"], "b");
        assert_eq!(w["failure"]["contradicted"], true);
        assert!(
            w["failure"]["reason"]
                .as_str()
                .unwrap()
                .contains("recorded error WorkFault")
        );
        assert_eq!(w["steps"][2]["output"]["text"], "y");
    }
    #[test]
    fn unlocated_refusal_after_consistent_steps_is_a_workflow_level_stop() {
        let run = json!({"status":"ERROR","output":null,"steps":[

            {"function_id":1,"function_name":"forge.flow.root/step:a","output":{"text":"x"},"error":null},

            {"function_id":2,"function_name":"forge.flow.root/step:b","output":{"ok":true},"error":null},

            {"function_id":3,"function_name":"forge.flow.root/step:c","output":{"text":"y"},"error":null}],

            "logs":[{"action":"flow_refused","detail":"root: output reference failed: x"}]});
        let w = workflow_steps(&def(), &run).unwrap();
        assert_eq!(states(&w), ["completed", "completed", "completed"]);
        assert_eq!(
            w["failure"],
            json!({"location":"workflow","step":null,"index":null,"reason":"root: output reference failed: x"})
        );
        assert_eq!(w["uncertain"], false);
    }
    #[test]
    fn active_runs_report_running_and_pending_and_actions_follow_status() {
        let run = json!({"status":"PENDING","output":null,"steps":[
            {"function_id":1,"function_name":"forge.flow.root/step:a","output":null,"error":null}],"logs":[]});
        let w = workflow_steps(&def(), &run).unwrap();
        assert_eq!(w["steps"][0]["state"], "running");
        assert_eq!(w["steps"][1]["state"], "pending");
        assert!(w["failure"].is_null());
        assert_eq!(actions("PENDING")["cancel"], true);
        assert_eq!(actions("ERROR")["retry"], true);
        assert_eq!(
            actions("SUCCESS"),
            json!({"cancel":false,"retry":false,"reconcile":false})
        );
    }
}

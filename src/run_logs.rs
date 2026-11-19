//! Bounded, paginated, redacted run logs and the authenticated live run-event stream.
//!
//! Both read only retained evidence: the audit trail, each harness step's private
//! `attempt.json` diagnostics and its streamed ACP `updates.jsonl`. Everything leaving
//! this module is scrubbed with every secret the service knows (actor tokens, model
//! credentials from the environment, runner secrets) and bounded per entry and per page.
use crate::{
    core::{Core, Invalid},
    harness::scrub,
};
use anyhow::Result;
use axum::body::Bytes;
use rusqlite::params;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration, time::Instant};

pub const PAGE_DEFAULT: i64 = 100;
pub const PAGE_MAX: i64 = 200;
const ENTRY_LIMIT: usize = 4096;
const STREAM_TICK: Duration = Duration::from_millis(400);
const STREAM_HEARTBEAT: Duration = Duration::from_secs(15);
/// One stream connection lives at most this long; the client reconnects (events carry
/// stable ids, so it never shows one twice).
const STREAM_LIFETIME: Duration = Duration::from_secs(10 * 60);
const TERMINAL: [&str; 4] = [
    "SUCCESS",
    "ERROR",
    "CANCELLED",
    "MAX_RECOVERY_ATTEMPTS_EXCEEDED",
];

fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut cut = limit;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}
/// A JSON value whose serialization exceeds the per-entry bound becomes a marked preview.
fn bounded(value: Value) -> Value {
    let text = value.to_string();
    if text.len() <= ENTRY_LIMIT {
        value
    } else {
        json!({"truncated":true,"bytes":text.len(),"preview":clip(&text,512)})
    }
}

impl Core {
    /// Every value that must never be shown: actor tokens, configured model credentials
    /// read from the environment, and runner secrets.
    pub(crate) fn redactions(&self) -> Vec<String> {
        let mut secrets: Vec<String> = self
            .policy
            .actors
            .values()
            .map(|a| a.token.clone())
            .collect();
        for model in self.policy.models.values() {
            if let Some(value) = model.key_env.as_deref().and_then(|n| std::env::var(n).ok()) {
                secrets.push(value);
            }
        }
        for runner in self.policy.runners.values() {
            secrets.extend(runner.secrets());
        }
        secrets
    }
    fn redact(&self, text: &str) -> String {
        scrub(text, &self.redactions())
    }
    pub(crate) fn redact_value(&self, value: &Value) -> Value {
        serde_json::from_str(&self.redact(&value.to_string())).unwrap_or(Value::Null)
    }

    /// The harness steps of a run: (step slot, invocation id).
    pub(crate) fn harness_steps(&self, run_id: &str) -> Result<Vec<(String, String)>> {
        let slots = self.store.write(|c| {
            let mut q = c.prepare("SELECT slot FROM forge_flow_effects WHERE run_id=?1 AND json_extract(action,'$.type')='harness' ORDER BY rowid")?;
            Ok(q.query_map([run_id], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        Ok(slots
            .into_iter()
            .map(|slot| {
                let invocation = Self::harness_invocation(run_id, &slot, 1);
                (slot, invocation)
            })
            .collect())
    }
    fn harness_attempt_value(&self, invocation: &str) -> Option<Value> {
        let bytes = std::fs::read(self.harness_dir(invocation).join("attempt.json")).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
    fn update_lines(&self, invocation: &str) -> Vec<String> {
        std::fs::read_to_string(self.harness_dir(invocation).join("updates.jsonl"))
            .map(|t| t.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }
    fn update_entry(&self, step: &str, index: usize, line: &str) -> Value {
        let mut parsed: Value = serde_json::from_str(&self.redact(line)).unwrap_or(Value::Null);
        let kind = parsed["kind"].as_str().unwrap_or("unknown").to_owned();
        json!({
            "id": format!("{step}#{index}"),
            "step": step,
            "seq": index,
            "ms": parsed["ms"],
            "type": kind,
            "body": bounded(parsed["body"].take()),
        })
    }

    /// Logs and diagnostics of one run, in sources: `audit` and one `harness:<step>` per
    /// harness step. Without `source` the call returns the index (each source's entry
    /// count plus per-attempt diagnostics). With one, it returns a bounded page after
    /// the `after` cursor; `next_after` continues it and `complete` says whether the
    /// retained evidence is exhausted.
    pub async fn run_logs(
        &self,
        actor: &str,
        run_id: &str,
        source: Option<&str>,
        after: Option<i64>,
        limit: Option<i64>,
    ) -> Result<Value> {
        self.owned(actor, run_id)?;
        let after = after.unwrap_or(0);
        if after < 0 {
            return Err(Invalid("after must not be negative").into());
        }
        let limit = limit.unwrap_or(PAGE_DEFAULT).clamp(1, PAGE_MAX) as usize;
        let steps = self.harness_steps(run_id)?;
        let audit_count: i64 = self.store.write(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM forge_audit WHERE target=?1",
                [run_id],
                |r| r.get(0),
            )?)
        })?;
        let Some(source) = source else {
            let mut sources = vec![json!({"source":"audit","entries":audit_count})];
            for (step, invocation) in &steps {
                let attempt = self
                    .harness_attempt_value(invocation)
                    .map(|a| self.redact_value(&a));
                sources.push(json!({
                    "source": format!("harness:{step}"),
                    "step": step,
                    "entries": self.update_lines(invocation).len(),
                    "attempts": [{
                        "attempt": 1,
                        "invocation": invocation,
                        "state": attempt.as_ref().map(|a| a["state"].clone()),
                        "result": attempt.as_ref().map(|a| a["result"].clone()),
                        "failure": attempt.as_ref().map(|a| a["failure"].clone()),
                        "protocol_error": attempt.as_ref().map(|a| a["protocol_error"].clone()),
                        "exit_code": attempt.as_ref().map(|a| a["exit_code"].clone()),
                        "stderr_tail": attempt.as_ref().map(|a| a["stderr_tail"].clone()),
                        "tool_calls": attempt.as_ref().map(|a| a["tool_calls"].clone()),
                        "permission_decisions": attempt.as_ref().map(|a| a["permission_decisions"].clone()),
                    }],
                }));
            }
            return Ok(json!({"run_id":run_id,"sources":sources}));
        };
        let (entries, total): (Vec<Value>, usize) = if source == "audit" {
            let rows = self.store.write(|c| {
                let mut q = c.prepare("SELECT actor,action,detail,created FROM forge_audit WHERE target=?1 ORDER BY id LIMIT ?2 OFFSET ?3")?;
                Ok(q.query_map(params![run_id, limit as i64, after], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, f64>(3)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?)
            })?;
            (
                rows.into_iter()
                    .enumerate()
                    .map(|(i, (a, action, detail, created))| {
                        json!({"id":format!("audit#{}",after as usize + i),"seq":after as usize + i,"ms":(created*1000.0) as i64,"type":action,"body":{"actor":a,"detail":clip(&self.redact(&detail),ENTRY_LIMIT)}})
                    })
                    .collect(),
                audit_count as usize,
            )
        } else if let Some(step) = source.strip_prefix("harness:") {
            let (_, invocation) = steps
                .iter()
                .find(|(s, _)| s == step)
                .ok_or(Invalid("unknown log source for this run"))?;
            let lines = self.update_lines(invocation);
            let total = lines.len();
            (
                lines
                    .iter()
                    .enumerate()
                    .skip(after as usize)
                    .take(limit)
                    .map(|(i, l)| self.update_entry(step, i, l))
                    .collect(),
                total,
            )
        } else {
            return Err(Invalid("source must be audit or harness:<step>").into());
        };
        let next = after as usize + entries.len();
        Ok(json!({
            "run_id": run_id,
            "source": source,
            "entries": entries,
            "next_after": next,
            "complete": next >= total,
            "total": total,
        }))
    }

    async fn stream_status(&self, run_id: &str) -> Option<String> {
        let rows = self
            .client
            .get()?
            .list_workflows(&dbos::sysdb::types::WorkflowFilter {
                workflow_ids: vec![run_id],
                load_input: false,
                load_output: false,
                ..Default::default()
            })
            .await
            .ok()?;
        rows.first().map(|r| r.status.as_str().to_owned())
    }
    fn effect_states(&self, run_id: &str) -> Vec<(String, String)> {
        self.store
            .write(|c| {
                let mut q = c.prepare(
                    "SELECT slot,state FROM forge_flow_effects WHERE run_id=?1 ORDER BY rowid",
                )?;
                Ok(q.query_map([run_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .unwrap_or_default()
    }
    fn approval_states(&self, run_id: &str) -> Vec<Value> {
        self.store
            .write(|c| {
                let mut q = c.prepare("SELECT id,slot,title,state,actor,note FROM forge_approvals WHERE run_id=?1 ORDER BY rowid")?;
                Ok(q.query_map([run_id], |r| {
                    Ok(json!({"approval_id":r.get::<_,String>(0)?,"step":r.get::<_,String>(1)?,"title":r.get::<_,String>(2)?,"state":r.get::<_,String>(3)?,"actor":r.get::<_,Option<String>>(4)?,"note":r.get::<_,Option<String>>(5)?}))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .unwrap_or_default()
    }
}

/// One server-sent-events connection for one run: state transitions, effect states,
/// approvals, and the streamed harness/ACP updates (agent text, tool events, permission
/// requests and decisions), each with a stable id. Ends after the run is terminal and
/// its evidence has been drained, or after [`STREAM_LIFETIME`].
pub(crate) struct RunStream {
    core: Arc<Core>,
    run_id: String,
    started: Instant,
    last_write: Instant,
    status: Option<String>,
    effects: BTreeMap<String, String>,
    approvals: BTreeMap<String, String>,
    offsets: BTreeMap<String, usize>,
    draining: bool,
    done: bool,
    first: bool,
}

fn frame(id: Option<&str>, event: &str, data: &Value) -> Bytes {
    let mut out = String::new();
    if let Some(id) = id {
        out.push_str(&format!("id: {id}\n"));
    }
    out.push_str(&format!("event: {event}\ndata: {data}\n\n"));
    Bytes::from(out)
}

impl RunStream {
    pub(crate) fn new(core: Arc<Core>, run_id: String) -> Self {
        Self {
            core,
            run_id,
            started: Instant::now(),
            last_write: Instant::now(),
            status: None,
            effects: BTreeMap::new(),
            approvals: BTreeMap::new(),
            offsets: BTreeMap::new(),
            draining: false,
            done: false,
            first: true,
        }
    }
    /// The next batch of frames (possibly empty); `None` when the stream is over.
    pub(crate) async fn next_batch(&mut self) -> Option<Vec<Bytes>> {
        if self.done {
            return None;
        }
        if !self.first {
            tokio::time::sleep(STREAM_TICK).await;
        }
        let mut out = Vec::new();
        let mut status = self.core.stream_status(&self.run_id).await;
        let effects = self.core.effect_states(&self.run_id);
        let uncertain = effects.iter().any(|(_, s)| s == "uncertain");
        let active = status.as_deref().is_none_or(|s| !TERMINAL.contains(&s));
        if uncertain && !active {
            status = Some("RECOVERY_REQUIRED".into());
        }
        if status != self.status || self.first {
            out.push(frame(
                None,
                "state",
                &json!({"status":status,"effects":effects.iter().map(|(s,st)|json!({"step":s,"state":st})).collect::<Vec<_>>()}),
            ));
            self.status = status.clone();
        } else {
            for (slot, state) in &effects {
                if self.effects.get(slot) != Some(state) {
                    out.push(frame(None, "effect", &json!({"step":slot,"state":state})));
                }
            }
        }
        self.effects = effects.into_iter().collect();
        for a in self.core.approval_states(&self.run_id) {
            let id = a["approval_id"].as_str().unwrap_or("").to_owned();
            let state = a["state"].as_str().unwrap_or("").to_owned();
            if self.approvals.get(&id) != Some(&state) {
                out.push(frame(None, "approval", &a));
                self.approvals.insert(id, state);
            }
        }
        if let Ok(steps) = self.core.harness_steps(&self.run_id) {
            for (step, invocation) in steps {
                let lines = self.core.update_lines(&invocation);
                let seen = self.offsets.entry(step.clone()).or_insert(0);
                for (i, line) in lines.iter().enumerate().skip(*seen) {
                    let entry = self.core.update_entry(&step, i, line);
                    let id = entry["id"].as_str().unwrap_or("").to_owned();
                    let kind = entry["type"].as_str().unwrap_or("update").to_owned();
                    out.push(frame(Some(&id), &kind, &entry));
                }
                *seen = lines.len();
            }
        }
        self.first = false;
        if !active {
            if self.draining && out.is_empty() {
                out.push(frame(None, "end", &json!({"status":self.status})));
                self.done = true;
                return Some(out);
            }
            self.draining = true;
        }
        if self.started.elapsed() > STREAM_LIFETIME {
            out.push(frame(
                None,
                "end",
                &json!({"status":self.status,"reconnect":true}),
            ));
            self.done = true;
            return Some(out);
        }
        if out.is_empty() && self.last_write.elapsed() >= STREAM_HEARTBEAT {
            out.push(Bytes::from_static(b": heartbeat\n\n"));
        }
        if !out.is_empty() {
            self.last_write = Instant::now();
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{self, Core};
    use crate::dbos::admission::AdmissionStore;
    use crate::settings::Paths;
    use std::collections::BTreeMap;

    fn core() -> (tempfile::TempDir, Arc<Core>) {
        let home = tempfile::tempdir().unwrap();
        core::init(home.path()).unwrap();
        let policy = core::load_policy(home.path()).unwrap();
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

    #[test]
    fn entries_are_bounded_and_secrets_are_redacted_everywhere() {
        let (_home, core) = core();
        let token = core.policy.actors["operator"].token.clone();
        let line = json!({"ms":1,"kind":"agent_message_chunk","body":{"content":{"type":"text","text":format!("the token is {token} ok")}}}).to_string();
        let entry = core.update_entry("root/step:h", 3, &line);
        assert_eq!(entry["id"], "root/step:h#3");
        let shown = entry.to_string();
        assert!(!shown.contains(&token), "{shown}");
        assert!(shown.contains("[redacted]"));
        let big = json!({"ms":1,"kind":"tool_call","body":{"raw":"x".repeat(10_000)}}).to_string();
        let entry = core.update_entry("s", 0, &big);
        assert_eq!(entry["body"]["truncated"], true);
        assert!(entry.to_string().len() < 2000);
        // Frames carry a stable id and one JSON data line (no raw newlines).
        let f = frame(Some("a#1"), "tool_call", &json!({"k":"line1\nline2"}));
        let text = String::from_utf8(f.to_vec()).unwrap();
        assert!(text.starts_with("id: a#1\nevent: tool_call\ndata: {"));
        assert_eq!(text.matches('\n').count(), 4);
    }
}

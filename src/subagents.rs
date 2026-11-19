//! Subagent operations: thin wrappers over harness work. A subagent is one admitted
//! `harness` run, never a new engine or concept. `spawn` registers (once) a fixed,
//! immutable work definition per runner profile and permission ceiling whose prompt and
//! instructions are invocation parameters, then admits a run of it with the caller's
//! idempotency key. `poll`, `list` and `cancel` read or fence that same run through the
//! existing inspection, agents and cancel operations, with the same authority checks.
use crate::core::{Core, Invalid, hash};
use anyhow::Result;
use serde_json::{Value, json};

/// Work ids of subagent definitions all start with this prefix.
pub const PREFIX: &str = "subagent.";
const MAX_TEXT: usize = 65536;
const POLL_DEFAULT: usize = 8192;
const POLL_MAX: usize = 65536;
const ACTIVE: [&str; 4] = ["ADMITTED", "PENDING", "ENQUEUED", "DELAYED"];

fn default_schema() -> Value {
    json!({"type":"object","properties":{"result":{"type":"string","maxLength":MAX_TEXT}},"required":["result"],"additionalProperties":false})
}

/// Arguments of one subagent spawn.
pub struct Spawn<'a> {
    pub profile: &'a str,
    pub prompt: &'a str,
    pub instructions: Option<&'a str>,
    pub key: &'a str,
    pub permissions: Option<&'a str>,
    pub schema: Option<&'a Value>,
}

impl Core {
    /// The deterministic work identity and definition of a subagent shape.
    pub(crate) fn subagent_definition(
        profile: &str,
        permissions: &str,
        schema: Option<&Value>,
    ) -> Result<(String, Value)> {
        let mut id = format!("{PREFIX}{profile}.{permissions}");
        let schema = match schema {
            Some(schema) => {
                id.push('.');
                id.push_str(&hash(crate::canonical::to_vec(schema)?)[..10]);
                schema.clone()
            }
            None => default_schema(),
        };
        crate::core::identifier(&id)
            .map_err(|_| Invalid("runner profile name is too long or invalid for a subagent"))?;
        let definition = json!({"type":"harness","input":{
            "profile":profile,
            "permissions":permissions,
            "prompt":"${/params/prompt}",
            "instructions":"${/params/instructions}",
            "output_schema":schema}});
        Ok((id, definition))
    }

    /// Admit one subagent run. The runner profile must be an operator-approved profile
    /// and within the actor's authority (the `harness` type, `register` once for a new
    /// shape, `execute` always). The same key repeats the same run; a new key is a new run.
    pub async fn subagent_spawn(&self, actor: &str, spawn: Spawn<'_>) -> Result<Value> {
        let Spawn {
            profile,
            prompt,
            instructions,
            key,
            permissions,
            schema,
        } = spawn;
        self.permit(actor, "execute", Some("harness"))?;
        let permissions = permissions.unwrap_or("read-only");
        if !["read-only", "workspace-write"].contains(&permissions) {
            return Err(Invalid("permissions must be read-only or workspace-write").into());
        }
        if prompt.trim().is_empty() || prompt.len() > MAX_TEXT {
            return Err(Invalid("prompt is required (at most 64 KiB)").into());
        }
        if instructions.is_some_and(|i| i.len() > MAX_TEXT) {
            return Err(Invalid("instructions are at most 64 KiB").into());
        }
        if let Some(schema) = schema
            && !schema.is_object()
        {
            return Err(Invalid("output_schema must be a JSON object").into());
        }
        self.runner(profile)?;
        let (work_id, definition) = Self::subagent_definition(profile, permissions, schema)?;
        match self.work(actor, &work_id) {
            Ok(existing) => {
                if existing["definition"] != definition {
                    return Err(Invalid("a subagent definition with this identity differs").into());
                }
            }
            Err(_) => {
                self.register(actor, &work_id, definition)?;
            }
        }
        let params = json!({"prompt":prompt,"instructions":instructions.unwrap_or("")});
        let run = self
            .run_with_params(actor, &work_id, key, None, Some(params))
            .await?;
        self.record(
            actor,
            "subagent_spawn",
            run["run_id"].as_str().unwrap_or(""),
            profile,
        )?;
        Ok(
            json!({"run_id":run["run_id"],"work_id":work_id,"profile":profile,"permissions":permissions,"key":key}),
        )
    }

    /// The subagent work id of one of the actor's runs, or a refusal.
    fn subagent_work(&self, actor: &str, run_id: &str) -> Result<String> {
        let envelope = self.owned(actor, run_id)?;
        let work = envelope.invocation.source_value()["workspace"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        if !work.starts_with(PREFIX) {
            return Err(Invalid("run is not a subagent run (spawned by subagent_spawn)").into());
        }
        Ok(work)
    }

    /// Status plus bounded output of one subagent run. Read-only.
    pub async fn subagent_poll(
        &self,
        actor: &str,
        run_id: &str,
        max_bytes: Option<i64>,
    ) -> Result<Value> {
        self.permit(actor, "read", None)?;
        let work = self.subagent_work(actor, run_id)?;
        let limit = max_bytes
            .map(|m| m.clamp(256, POLL_MAX as i64) as usize)
            .unwrap_or(POLL_DEFAULT);
        let run = self.inspect(actor, run_id).await?;
        let status = run["status"].as_str().unwrap_or("").to_owned();
        let text = run["output"].to_string();
        let output = if text.len() <= limit {
            run["output"].clone()
        } else {
            json!({"truncated":true,"bytes":text.len(),"preview":text.chars().take(limit).collect::<String>()})
        };
        let effect = run["effects"]
            .as_array()
            .and_then(|e| e.first())
            .map(|e| json!({"effect_id":e["effect_id"],"state":e["state"]}));
        Ok(json!({
            "run_id": run_id,
            "work_id": work,
            "status": status,
            "done": !ACTIVE.contains(&status.as_str()),
            "output": output,
            "error": run["error"],
            "diagnostic": run["diagnostic"],
            "effect": effect,
            "actions": run["actions"],
            "next": if ACTIVE.contains(&status.as_str()) { "poll again, or cancel" } else { "use runs inspect / run_logs for steps and diagnostics" },
        }))
    }

    /// The actor's subagent runs (active first, then recent). Read-only.
    pub async fn subagent_list(&self, actor: &str, limit: Option<i64>) -> Result<Value> {
        let all = self.agents(actor, Some(50)).await?;
        let limit = limit.unwrap_or(25).clamp(1, 50) as usize;
        let pick = |key: &str| -> Vec<Value> {
            all[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|r| r["work_id"].as_str().is_some_and(|w| w.starts_with(PREFIX)))
                .take(limit)
                .cloned()
                .collect()
        };
        Ok(json!({"active":pick("active"),"recent":pick("recent")}))
    }

    /// Cancel one subagent run through the existing cooperative cancellation fence.
    pub async fn subagent_cancel(&self, actor: &str, run_id: &str) -> Result<Value> {
        self.subagent_work(actor, run_id)?;
        self.cancel(actor, run_id).await
    }
}

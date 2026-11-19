//! Per-call observations for multi-step flows; DBOS alone owns execution progress.
use crate::{
    core::{Core, Invalid, hash},
    dbos::admission::Envelope,
    safety,
};
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

impl Core {
    pub(crate) fn flow_fenced(&self, e: &Envelope) -> Result<bool> {
        self.store.verify(e)?;
        Ok(self.store.write(|c| {
            Ok(c.query_row(
                "SELECT 1 FROM forge_run_fences WHERE run_id=?1",
                [&e.dbos_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
        })?)
    }
    pub(crate) fn fence_flow(&self, e: &Envelope) -> Result<()> {
        self.store.verify(e)?;
        self.store.write(|c| {
            c.execute(
                "INSERT OR IGNORE INTO forge_run_fences(run_id) VALUES(?1)",
                [&e.dbos_id],
            )?;
            Ok(())
        })?;
        Ok(())
    }
    pub(crate) fn flow_current(&self, e: &Envelope) -> Result<Value> {
        let definition = self.current(e)?;
        if self.flow_fenced(e)? {
            return Err(Invalid("flow is permanently fenced from further dispatch").into());
        }
        Ok(definition)
    }
    fn flow_output_path(&self, run: &str, slot: &str) -> std::path::PathBuf {
        self.paths.outputs.join(format!(
            "flow-{}.json",
            hash(crate::canonical::to_vec(&json!([run, slot])).expect("string tuple"))
        ))
    }
    pub(crate) fn flow_receipt(
        &self,
        e: &Envelope,
        slot: &str,
        digest: &str,
        output: Value,
    ) -> Result<Value> {
        let body = json!({"run_id":e.dbos_id,"invocation":e.invocation_sha256,"slot":slot,"action_digest":digest,"output":output});
        let bytes = crate::canonical::to_vec(&body)?;
        if bytes.len() > 4 * 1024 * 1024 {
            bail!("flow output exceeds bounded receipt size");
        }
        let path = self.flow_output_path(&e.dbos_id, slot);
        if path.exists() {
            if safety::read_private(&path)? != bytes {
                bail!("immutable flow output conflict");
            }
        } else {
            safety::write_new(&path, &bytes)?;
        }
        Ok(json!({"sha256":hash(bytes),"slot":slot}))
    }
    pub(crate) fn flow_read_output(
        &self,
        e: &Envelope,
        slot: &str,
        digest: &str,
        receipt: &Value,
    ) -> Result<Value> {
        let bytes = safety::read_private(&self.flow_output_path(&e.dbos_id, slot))?;
        if receipt["sha256"] != hash(&bytes) || receipt["slot"] != slot {
            bail!("flow receipt integrity failed");
        }
        let body: Value = serde_json::from_slice(&bytes)?;
        if body["run_id"] != e.dbos_id
            || body["invocation"] != e.invocation_sha256
            || body["slot"] != slot
            || body["action_digest"] != digest
        {
            bail!("flow receipt identity mismatch");
        }
        Ok(body["output"].clone())
    }
    pub(crate) fn flow_effect_observations(&self, e: &Envelope) -> Result<Vec<Value>> {
        self.store.verify(e)?;
        let rows=self.store.write(|c| {
            let mut query=c.prepare("SELECT slot,action_digest,state,receipt,action,decision FROM forge_flow_effects WHERE run_id=?1 ORDER BY rowid")?;
            Ok(query.query_map([&e.dbos_id],|r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?)))?.collect::<std::result::Result<Vec<_>,_>>()?)
        })?;
        rows.into_iter().map(|(slot,digest,state,receipt,action,decision)| {
            let output=if let Some(receipt)=receipt {self.flow_read_output(e,&slot,&digest,&serde_json::from_str(&receipt)?)?}else{Value::Null};
            let action=serde_json::from_str::<Value>(&action)?;
            let attempt=if action["type"]=="harness" {self.harness_attempt(&e.dbos_id,&slot)} else {None};
            Ok(json!({"effect_id":slot,"state":state,"action_digest":digest,"action":action,"output":output,"harness_attempt":attempt,"reconciliation":decision.map(|v|serde_json::from_str::<Value>(&v)).transpose()?}))
        }).collect()
    }
    /// The caller supplies an actual resolved action; parent and leaf authority
    /// are revalidated before every outbound call and cached-result publication.
    pub(crate) async fn flow_effect(
        &self,
        e: &Envelope,
        slot: &str,
        action: &Value,
    ) -> Result<Value> {
        if slot.is_empty() || slot.len() > 512 {
            return Err(Invalid("invalid flow effect identity").into());
        }
        let _guard = self.effect_lock(&e.dbos_id)?;
        self.flow_current(e)?;
        self.validate_leaf(e.invocation.principal(), action, "execute")?;
        let digest = self.action_hash(e.invocation.principal(), action)?;
        if action["type"] == "harness" {
            let fresh = self.store.write(|c| {
                Ok(c.query_row(
                    "SELECT 1 FROM forge_flow_effects WHERE run_id=?1 AND slot=?2",
                    params![e.dbos_id, slot],
                    |_| Ok(()),
                )
                .optional()?
                .is_none())
            })?;
            // Unavailable harness or containment is a refusal, never an uncertain effect:
            // nothing may be recorded as possibly-launched before these checks pass.
            if fresh {
                self.harness_preflight(&action["input"]).await?;
            }
        }
        let raw = crate::canonical::to_string(action)?;
        if raw.len() > 1024 * 1024 {
            return Err(Invalid("resolved flow action exceeds 1 MiB budget").into());
        }
        let previous=self.store.write(|c|{
            let old=c.query_row("SELECT invocation_digest,action_digest,state,receipt,action FROM forge_flow_effects WHERE run_id=?1 AND slot=?2",params![e.dbos_id,slot],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,String>(4)?))).optional()?;
            if old.is_none() { c.execute("INSERT INTO forge_flow_effects(run_id,slot,invocation_digest,action_digest,state,action) VALUES(?1,?2,?3,?4,'uncertain',?5)",params![e.dbos_id,slot,e.invocation_sha256,digest,raw])?; }
            Ok(old)
        })?;
        if let Some((inv, old_digest, state, receipt, old_action)) = previous {
            if inv != e.invocation_sha256 || old_digest != digest || old_action != raw {
                bail!("immutable flow effect binding changed");
            }
            if state != "completed" {
                return Err(Invalid(
                    "flow effect outcome is unresolved or fenced; inspect exact effect evidence",
                )
                .into());
            }
            let output = self.flow_read_output(
                e,
                slot,
                &digest,
                &serde_json::from_str(&receipt.context("flow receipt missing")?)?,
            )?;
            self.flow_current(e)?;
            return Ok(output);
        }
        self.flow_current(e)?;
        self.validate_leaf(e.invocation.principal(), action, "execute")?;
        let effect_key = format!(
            "forge-flow-effect-{}",
            hash(crate::canonical::to_vec(&json!([e.dbos_id, slot, digest]))?)
        );
        self.record("runtime", "flow_effect_intent", &e.dbos_id, slot)?;
        let output = self.flow_outbound(e, slot, action, &effect_key).await?;
        let receipt = self.flow_receipt(e, slot, &digest, output.clone())?;
        self.store.write(|c| {
            let count=c.execute("UPDATE forge_flow_effects SET state='completed',receipt=?3 WHERE run_id=?1 AND slot=?2 AND invocation_digest=?4 AND action_digest=?5 AND state='uncertain'",params![e.dbos_id,slot,serde_json::to_string(&receipt).expect("receipt"),e.invocation_sha256,digest])?;
            if count!=1 {return Err(crate::dbos::admission::Error::Invalid("flow effect lost exact intent"));} Ok(())
        })?;
        self.record("runtime", "flow_effect_completed", &e.dbos_id, slot)?;
        self.flow_current(e)?;
        Ok(output)
    }
    pub(crate) fn flow_verify_result(
        &self,
        e: &Envelope,
        slot: &str,
        action: &Value,
        output: &Value,
    ) -> Result<()> {
        self.flow_current(e)?;
        let digest = self.action_hash(e.invocation.principal(), action)?;
        let row=self.store.write(|c| Ok(c.query_row("SELECT invocation_digest,action_digest,state,receipt FROM forge_flow_effects WHERE run_id=?1 AND slot=?2",params![e.dbos_id,slot],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?))).optional()?))?.context("cached flow step lacks receipt")?;
        if row.0 != e.invocation_sha256 || row.1 != digest || row.2 != "completed" {
            bail!("cached flow step effect binding mismatch");
        }
        let retained = self.flow_read_output(
            e,
            slot,
            &digest,
            &serde_json::from_str(&row.3.context("cached receipt missing")?)?,
        )?;
        if &retained != output {
            bail!("cached flow step output differs from receipt");
        }
        Ok(())
    }
    pub(crate) async fn flow_outbound(
        &self,
        e: &Envelope,
        slot: &str,
        action: &Value,
        key: &str,
    ) -> Result<Value> {
        let kind = action["type"].as_str().context("action type missing")?;
        let input = &action["input"];
        if kind == "model" || kind == "agent_turn" {
            let model = self
                .policy
                .models
                .get(input["model"].as_str().unwrap_or(""))
                .context("model unavailable")?;
            let mut body = if kind == "agent_turn" {
                input["request"].clone()
            } else {
                json!({"messages":[{"role":"user","content":input["prompt"]}],"max_tokens":256,"temperature":0,"chat_template_kwargs":{"enable_thinking":false}})
            };
            body["model"] = json!(model.model);
            body["stream"] = json!(false);
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(90))
                .build()?;
            let mut request = client
                .post(format!(
                    "{}/chat/completions",
                    model.base_url.trim_end_matches('/')
                ))
                .json(&body);
            if let Some(key) = &model.key_env {
                request = request
                    .bearer_auth(std::env::var(key).context("model credential unavailable")?);
            }
            let mut response = request.send().await?.error_for_status()?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if bytes.len() + chunk.len() > 1024 * 1024 {
                    bail!("model response exceeded limit");
                }
                bytes.extend(chunk);
            }
            let response: Value = serde_json::from_slice(&bytes)?;
            if kind == "agent_turn" {
                let message = response["choices"][0]["message"]
                    .as_object()
                    .context("agent reply missing message")?;
                Ok(json!({"message":message,"model":response["model"],"usage":response["usage"]}))
            } else {
                let text = response["choices"][0]["message"]["content"]
                    .as_str()
                    .context("model reply malformed")?;
                Ok(json!({"text":text,"model":response["model"],"artifacts":[]}))
            }
        } else if kind == "harness" {
            self.run_harness(e, slot, action).await
        } else {
            let plugin = self.plugin(kind).context("plugin unavailable")?;
            let response = crate::plugin::invoke(
                &plugin.config,
                plugin.execute_request(input, &e.dbos_id, key),
            )
            .await?;
            response
                .get("output")
                .cloned()
                .context("plugin output missing")
        }
    }
    pub(crate) fn reconcile_flow(
        &self,
        e: &Envelope,
        effect_id: Option<&str>,
        decision: &str,
        note: &str,
        output: Value,
    ) -> Result<()> {
        let _guard = self.effect_lock(&e.dbos_id)?;
        let effects = self.flow_effect_observations(e)?;
        let choices: Vec<_> = effects
            .iter()
            .filter(|v| {
                v["state"] == "uncertain" && effect_id.is_none_or(|id| v["effect_id"] == id)
            })
            .collect();
        if choices.len() != 1 {
            return Err(Invalid("select one unresolved flow effect with effect_id").into());
        }
        let effect = choices[0];
        let slot = effect["effect_id"].as_str().context("effect id missing")?;
        let digest = effect["action_digest"]
            .as_str()
            .context("effect digest missing")?;
        let receipt = if decision == "completed" {
            Some(self.flow_receipt(e, slot, digest, output)?)
        } else {
            if self.flow_output_path(&e.dbos_id, slot).exists() {
                return Err(Invalid("recorded output contradicts not-applied evidence").into());
            }
            None
        };
        self.fence_flow(e)?;
        let evidence = json!({"actor":e.invocation.principal(),"decision":decision,"note":note,"created_ms":chrono::Utc::now().timestamp_millis()});
        self.store.write(|c|{
            let changed=c.execute("UPDATE forge_flow_effects SET state=?3,receipt=?4,decision=?5 WHERE run_id=?1 AND slot=?2 AND state='uncertain'",params![e.dbos_id,slot,decision,receipt.map(|v|v.to_string()),evidence.to_string()])?;
            if changed!=1 {return Err(crate::dbos::admission::Error::Invalid("flow effect no longer unresolved"));}Ok(())
        })?;
        self.record(e.invocation.principal(), "reconcile_flow", &e.dbos_id, slot)
    }
}

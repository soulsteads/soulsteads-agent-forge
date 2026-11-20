//! Flow definition binding and immutable invocation parameters.
use crate::{
    core::{Core, Invalid, hash},
    dbos::admission::Envelope,
    flow_schema,
};
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

fn dynamic_definition(definition: &Value) -> bool {
    if definition["type"] == "harness" {
        // The output schema is a literal JSON Schema, not a template.
        let mut input = definition["input"].clone();
        if let Some(map) = input.as_object_mut() {
            map.remove("output_schema");
        }
        return dynamic(&input);
    }
    dynamic(&definition["input"])
}
fn dynamic(value: &Value) -> bool {
    match value {
        Value::String(s) => s.contains("${"),
        Value::Array(a) => a.iter().any(dynamic),
        Value::Object(o) => o.contains_key("$ref") || o.values().any(dynamic),
        _ => false,
    }
}
pub(crate) fn is_flow(definition: &Value) -> bool {
    matches!(definition["type"].as_str(), Some("agent" | "workflow"))
}
/// Work whose external calls use the per-slot flow receipt ledger and fences.
pub(crate) fn is_ledgered(definition: &Value) -> bool {
    is_flow(definition) || definition["type"] == "harness"
}
/// Work that may receive invocation parameters.
pub(crate) fn accepts_params(definition: &Value) -> bool {
    is_ledgered(definition)
}
impl Core {
    pub(crate) fn validate_leaf(
        &self,
        actor: &str,
        definition: &Value,
        capability: &str,
    ) -> Result<()> {
        let kind = definition["type"]
            .as_str()
            .ok_or(Invalid("action type missing"))?;
        if kind == "agent_turn" {
            self.permit(actor, capability, Some("agent"))?;
            self.permit(actor, capability, Some("model"))?;
            if !self
                .policy
                .models
                .contains_key(definition["input"]["model"].as_str().unwrap_or(""))
            {
                return Err(Invalid("agent model unavailable").into());
            }
            return Ok(());
        }
        self.permit(actor, capability, Some(kind))?;
        if kind == crate::workflow_credentials::KIND {
            self.credential_grant(actor, definition)?;
            return Ok(());
        }
        if kind == "harness" {
            if !jsonschema::is_valid(
                &crate::flow_schema::harness_catalog()["schema"],
                &definition["input"],
            ) {
                return Err(Invalid("resolved harness input violates the workload schema").into());
            }
            self.validate_harness(&definition["input"])?;
            return Ok(());
        }
        let types = self.types();
        let schema = types.get(kind).ok_or(Invalid("unsupported action type"))?["schema"].clone();
        if !jsonschema::is_valid(&schema, &definition["input"]) {
            return Err(Invalid("resolved action input violates configured schema").into());
        }
        if kind == "model"
            && !self
                .policy
                .models
                .contains_key(definition["input"]["model"].as_str().unwrap_or(""))
        {
            return Err(Invalid("model unavailable").into());
        }
        Ok(())
    }
    pub(crate) fn validate_definition(
        &self,
        actor: &str,
        definition: &Value,
        capability: &str,
    ) -> Result<()> {
        if is_ledgered(definition) {
            flow_schema::validate(definition).map_err(|error| {
                let mut detail = format!("{error:#}");
                detail.truncate(detail.floor_char_boundary(1024));
                crate::core::InvalidFlow(detail)
            })?;
        }
        self.definition_binding(actor, definition, capability, 0, &mut 0)?;
        Ok(())
    }
    fn definition_binding(
        &self,
        actor: &str,
        definition: &Value,
        capability: &str,
        depth: usize,
        total: &mut usize,
    ) -> Result<Value> {
        if depth > 4 || *total >= 64 {
            return Err(Invalid("flow nesting or total action budget exceeded").into());
        }
        *total += 1;
        let kind = definition["type"]
            .as_str()
            .ok_or(Invalid("action type missing"))?;
        if kind == "agent_turn" {
            return Err(Invalid("internal agent turns cannot be authored as work").into());
        }
        self.permit(actor, capability, Some(kind))?;
        let input = &definition["input"];
        if !matches!(kind, "agent" | "workflow" | "agent_turn") && !dynamic_definition(definition) {
            self.validate_leaf(actor, definition, capability)?;
        }
        let destination = match kind {
            "agent" => {
                self.permit(actor, capability, Some("model"))?;
                let model = self
                    .policy
                    .models
                    .get(input["model"].as_str().unwrap_or(""))
                    .ok_or(Invalid("agent model alias unavailable"))?;
                let mut plugins = serde_json::Map::new();
                for tool in input["tools"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                    let ty = tool["type"]
                        .as_str()
                        .ok_or(Invalid("agent tool type missing"))?;
                    self.permit(actor, capability, Some(ty))?;
                    let plugin = self
                        .plugin(ty)
                        .ok_or(Invalid("agent tool plugin unavailable"))?;
                    plugins.insert(ty.to_owned(),json!({"config":plugin.config,"schema":plugin.schema,"description":plugin.description}));
                }
                json!({"model":model,"tools":plugins})
            }
            "workflow" => {
                let mut children = Vec::new();
                for step in input["steps"]
                    .as_array()
                    .ok_or(Invalid("workflow steps missing"))?
                {
                    let action = &step["action"];
                    let child = if let Some(id) = action["work_id"].as_str() {
                        let work = self.work(actor, id)?;
                        if work["enabled"] != true {
                            return Err(Invalid("referenced work disabled").into());
                        }
                        work["definition"].clone()
                    } else {
                        action.clone()
                    };
                    children.push(self.definition_binding(
                        actor,
                        &child,
                        capability,
                        depth + 1,
                        total,
                    )?);
                }
                json!(children)
            }
            "model" | "agent_turn" => serde_json::to_value(
                self.policy
                    .models
                    .get(input["model"].as_str().unwrap_or(""))
                    .ok_or(Invalid("model unavailable"))?,
            )?,
            "echo" | "fail" | "approval" => Value::Null,
            crate::workflow_credentials::KIND => {
                serde_json::to_value(self.credential_grant(actor, definition)?)?
            }
            "harness" => match input["profile"].as_str().filter(|p| !p.contains("${")) {
                // A templated profile name is resolved and bound again at its own dispatch.
                Some(name) => json!({"profile":name,"runner":self.runner(name)?}),
                None => Value::Null,
            },
            _ => {
                let plugin = self.plugin(kind).ok_or(Invalid("unknown action type"))?;
                serde_json::to_value(&plugin.config)?
            }
        };
        Ok(json!({"definition":definition,"destination":destination}))
    }
    pub(crate) fn action_hash(&self, actor: &str, definition: &Value) -> Result<String> {
        if definition["type"] == "agent_turn" {
            let model = self
                .policy
                .models
                .get(definition["input"]["model"].as_str().unwrap_or(""))
                .ok_or(Invalid("model unavailable"))?;
            return Ok(hash(crate::canonical::to_vec(
                &json!({"definition":definition,"destination":model}),
            )?));
        }
        Ok(hash(crate::canonical::to_vec(&self.definition_binding(
            actor, definition, "execute", 0, &mut 0,
        )?)?))
    }
    pub(crate) fn params_reference(&self, actor: &str, value: &Value) -> Result<Value> {
        if !value.is_object() {
            return Err(Invalid("invocation params must be a JSON object").into());
        }
        let raw = crate::canonical::to_string(value)?;
        if raw.len() > 1024 * 1024 {
            return Err(Invalid("invocation params exceed 1 MiB").into());
        }
        let digest = hash(&raw);
        self.store.write(|c| {
            c.execute(
                "INSERT OR IGNORE INTO forge_inputs(digest,payload) VALUES(?1,?2)",
                params![digest, raw],
            )?;
            let existing: String = c.query_row(
                "SELECT payload FROM forge_inputs WHERE digest=?1",
                [&digest],
                |r| r.get(0),
            )?;
            if existing != raw {
                return Err(crate::dbos::admission::Error::Invalid(
                    "immutable parameter payload conflict",
                ));
            }
            Ok(())
        })?;
        Ok(json!({"scope":actor,"object_id":format!("input:{digest}"),"sha256":digest}))
    }
    pub(crate) fn invocation_params(&self, e: &Envelope) -> Result<Value> {
        self.store.verify(e)?;
        let source = e.invocation.source_value();
        let reference = &source["parameters"]["input"];
        if reference.is_null() {
            return Ok(json!({}));
        }
        let digest = reference["sha256"]
            .as_str()
            .context("input digest missing")?;
        if reference["scope"] != e.invocation.scope()
            || reference["object_id"] != format!("input:{digest}")
        {
            return Err(Invalid("input reference scope mismatch").into());
        }
        let raw = self
            .store
            .write(|c| {
                Ok(c.query_row(
                    "SELECT payload FROM forge_inputs WHERE digest=?1",
                    [digest],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
            })?
            .context("input payload missing")?;
        if hash(&raw) != digest {
            return Err(Invalid("input payload integrity failed").into());
        }
        Ok(serde_json::from_str(&raw)?)
    }
    pub(crate) fn merge_params(&self, definition: &Value, explicit: &Value) -> Result<Value> {
        let mut result = definition["input"]
            .get("params")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let object = result
            .as_object_mut()
            .ok_or(Invalid("default params must be object"))?;
        for (key, value) in explicit
            .as_object()
            .ok_or(Invalid("params must be object"))?
        {
            object.insert(key.clone(), value.clone());
        }
        Ok(result)
    }
}

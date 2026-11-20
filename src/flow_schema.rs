//! Closed authoring contracts and data-only references for native DBOS flows.
//! This module performs no IO, provider lookup, permission grant or execution.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

pub const MAX_STEPS: usize = 32;
pub const MAX_TOTAL_STEPS: usize = 64;
pub const MAX_DEPTH: usize = 4;
pub const MAX_ROUNDS: usize = 8;
pub const MAX_TOOL_CALLS: usize = 32;
pub const MAX_CONTEXT_BYTES: usize = 1024 * 1024;
pub const MAX_STRING_BYTES: usize = 65536;
const MAX_JSON_DEPTH: usize = 32;

/// Input schemas advertised to every authoring client. Semantic dependency and
/// reference checks below supplement JSON Schema before a definition is stored.
pub fn catalog() -> Value {
    let identifier = json!({"type":"string","minLength":1,"maxLength":64,"pattern":"^[A-Za-z0-9_][A-Za-z0-9_.-]*$"});
    let definition = json!({"type":"object","properties":{"type":{"type":"string","minLength":1,"maxLength":128},"input":{"type":"object"}},"required":["type","input"],"additionalProperties":false});
    let reference = json!({"type":"object","properties":{"work_id":{"type":"string","minLength":1,"maxLength":128},"params":{"type":"object"}},"required":["work_id"],"additionalProperties":false});
    let workflow = json!({"type":"object","properties":{
        "params":{"type":"object"},
        "steps":{"type":"array","minItems":1,"maxItems":MAX_STEPS,"items":{"type":"object","properties":{
            "id":identifier,"needs":{"type":"array","maxItems":MAX_STEPS,"uniqueItems":true,"items":identifier},
            "action":{"oneOf":[definition,reference]}},"required":["id","action"],"additionalProperties":false}},
        "output":{}},"required":["steps"],"additionalProperties":false});
    let agent = json!({"type":"object","properties":{
        "model":{"type":"string","minLength":1,"maxLength":128},
        "instructions":{"type":"string","maxLength":MAX_STRING_BYTES},
        "prompt":{"type":"string","minLength":1,"maxLength":MAX_STRING_BYTES},
        "params":{"type":"object"},
        "tools":{"type":"array","maxItems":MAX_TOOL_CALLS,"items":{"type":"object","properties":{
            "name":{"type":"string","minLength":1,"maxLength":64,"pattern":"^[A-Za-z0-9_-]+$"},
            "type":{"type":"string","minLength":8,"maxLength":128,"pattern":"^plugin\\.[A-Za-z0-9_.-]+$"},
            "description":{"type":"string","maxLength":4096}},"required":["name","type"],"additionalProperties":false}},
        "max_rounds":{"type":"integer","minimum":1,"maximum":MAX_ROUNDS,"default":MAX_ROUNDS},
        "max_tool_calls":{"type":"integer","minimum":1,"maximum":MAX_TOOL_CALLS,"default":MAX_TOOL_CALLS},
        "output_schema":{"type":"object"}},"required":["model","prompt"],"additionalProperties":false});
    json!({"workflow":{"description":"Ordered durable steps with explicit dependencies, typed earlier-output references and optional named agent/workflow actions","schema":workflow},
        "agent":{"description":"Bounded durable model-selected tool loop using explicit configured plugin tools and optional structured output contract","schema":agent}})
}

/// Agent workload executed by an operator-approved native harness runner profile.
/// There is deliberately no field for an executable, arguments or environment.
pub fn harness_catalog() -> Value {
    let schema = json!({"type":"object","properties":{
        "profile":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_.-]+$"},
        "instructions":{"type":"string","maxLength":MAX_STRING_BYTES},
        "prompt":{"type":"string","minLength":1,"maxLength":MAX_STRING_BYTES},
        "inputs":{"type":"object"},
        "output_schema":{"type":"object"},
        "permissions":{"type":"string","enum":["read-only","workspace-write"],"default":"read-only"},
        "require":{"type":"array","maxItems":8,"uniqueItems":true,"items":{"type":"string","enum":crate::harness::CAPABILITIES}}},
        "required":["profile","prompt","output_schema"],"additionalProperties":false});
    json!({"description":"Typed agent workload run through a named operator-approved native harness profile (Codex, Claude Code, OpenCode); the final message is validated against output_schema","schema":schema})
}
/// Authored input without the literal JSON Schema, which is not a template.
pub fn without_schema(kind: &str, input: &Value) -> Value {
    let mut input = input.clone();
    if kind == "harness"
        && let Some(map) = input.as_object_mut()
    {
        map.remove("output_schema");
    }
    input
}
/// Render authored input; a harness output schema stays literal data.
pub fn render_input(kind: &str, input: &Value, context: &Value) -> Result<Value> {
    let mut rendered = render(&without_schema(kind, input), context)?;
    if kind == "harness"
        && let (Some(map), Some(schema)) = (rendered.as_object_mut(), input.get("output_schema"))
    {
        map.insert("output_schema".into(), schema.clone());
    }
    Ok(rendered)
}

fn object(value: &Value) -> Result<&Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("flow object required"))
}
fn closed(value: &Map<String, Value>, keys: &[&str]) -> Result<()> {
    ensure!(
        value.keys().all(|key| keys.contains(&key.as_str())),
        "unknown flow field"
    );
    Ok(())
}
fn name(value: &str, limit: usize, dotted: bool) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || (dotted && c == b'.'))
}
fn text(value: Option<&Value>, required: bool, limit: usize) -> Result<()> {
    match value {
        Some(Value::String(value)) if value.len() <= limit && (!required || !value.is_empty()) => {
            Ok(())
        }
        None if !required => Ok(()),
        _ => bail!("flow text is missing, invalid or exceeds its bound"),
    }
}
fn bounded_integer(value: Option<&Value>, maximum: usize) -> Result<()> {
    if let Some(value) = value {
        ensure!(
            value
                .as_u64()
                .is_some_and(|number| number > 0 && number <= maximum as u64),
            "flow budget must be a bounded positive integer"
        );
    }
    Ok(())
}

/// Validate structural semantics, including inline definitions. Named resource
/// expansion, aggregate bounds across named resources, provider availability
/// and actor authority are checked by the canonical service.
pub fn validate(definition: &Value) -> Result<()> {
    ensure!(
        serde_json::to_vec(definition)?.len() <= MAX_CONTEXT_BYTES,
        "flow definition exceeds size limit"
    );
    validate_definition(definition, 0, &mut 0)
}

fn validate_definition(definition: &Value, depth: usize, total: &mut usize) -> Result<()> {
    ensure!(depth <= MAX_DEPTH, "flow nesting limit exceeded");
    let definition = object(definition)?;
    closed(definition, &["type", "input"])?;
    let kind = definition
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("flow action type missing"))?;
    ensure!(name(kind, 128, true), "invalid flow action type");
    let input = object(
        definition
            .get("input")
            .ok_or_else(|| anyhow::anyhow!("flow input missing"))?,
    )?;
    match kind {
        "workflow" => validate_workflow(input, depth, total),
        "agent" => validate_agent(input),
        "harness" => validate_harness(input),
        "echo" | "fail" | "model" => Ok(()),
        "approval" => crate::approvals::validate_input(input),
        crate::workflow_credentials::KIND => {
            ensure!(
                depth == 1,
                "credential checks require a direct workflow step"
            );
            ensure!(
                input.len() == 1
                    && input
                        .get("credential")
                        .and_then(Value::as_str)
                        .is_some_and(crate::workflow_credentials::valid_alias),
                "credential checks require a literal credential alias"
            );
            Ok(())
        }
        kind if kind.starts_with("plugin.") && kind.len() > 7 => Ok(()),
        _ => bail!("unsupported authored flow action type"),
    }
}

fn validate_workflow(input: &Map<String, Value>, depth: usize, total: &mut usize) -> Result<()> {
    closed(input, &["params", "steps", "output"])?;
    if let Some(params) = input.get("params") {
        object(params)?;
    }
    let steps = input
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("workflow steps required"))?;
    ensure!(
        !steps.is_empty() && steps.len() <= MAX_STEPS,
        "workflow step count is out of bounds"
    );
    *total += steps.len();
    ensure!(
        *total <= MAX_TOTAL_STEPS,
        "aggregate workflow step limit exceeded"
    );
    let mut previous = BTreeSet::new();
    for (index, step_value) in steps.iter().enumerate() {
        // Locate diagnostics by authored position and (validated) step ID only;
        // never echo authored values back into an error.
        let label = match step_value.get("id").and_then(Value::as_str) {
            Some(id) if name(id, 64, true) => format!("step {} ({id})", index + 1),
            _ => format!("step {}", index + 1),
        };
        validate_step(step_value, &mut previous, depth, total).context(label)?;
    }
    if let Some(output) = input.get("output") {
        validate_references(output, &previous).context("output mapping")?;
    }
    Ok(())
}

fn validate_step(
    step: &Value,
    previous: &mut BTreeSet<String>,
    depth: usize,
    total: &mut usize,
) -> Result<()> {
    {
        let step = object(step)?;
        closed(step, &["id", "needs", "action"])?;
        let id = step
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("workflow step ID required"))?;
        ensure!(
            name(id, 64, true) && !id.starts_with(['.', '-']),
            "invalid workflow step ID"
        );
        ensure!(!previous.contains(id), "duplicate workflow step ID");
        let mut allowed = BTreeSet::new();
        if let Some(needs) = step.get("needs") {
            for dependency in needs
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("needs must be an array"))?
            {
                let dependency = dependency
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("dependency must be a step ID"))?;
                ensure!(
                    previous.contains(dependency) && allowed.insert(dependency.to_owned()),
                    "needs must contain unique earlier step IDs"
                );
            }
        }
        let action_value = step
            .get("action")
            .ok_or_else(|| anyhow::anyhow!("step action required"))?;
        let action = object(action_value)?;
        if let Some(work_id) = action.get("work_id") {
            closed(action, &["work_id", "params"])?;
            ensure!(
                work_id.as_str().is_some_and(|id| name(id, 128, true)),
                "invalid referenced work ID"
            );
            if let Some(params) = action.get("params") {
                object(params)?;
                validate_references(params, &allowed)?;
            }
        } else {
            validate_definition(action_value, depth + 1, total)?;
            let nested_input = &action["input"];
            if matches!(action["type"].as_str(), Some("harness")) {
                validate_references(&without_schema("harness", nested_input), &allowed)?;
            } else if matches!(action["type"].as_str(), Some("workflow" | "agent")) {
                // Child prompts/steps have a fresh scope. Only the child's
                // parameter binding refers to the parent step context.
                if let Some(params) = nested_input.get("params") {
                    validate_references(params, &allowed)?;
                }
            } else {
                validate_references(nested_input, &allowed)?;
            }
        }
        previous.insert(id.to_owned());
    }
    Ok(())
}

fn validate_harness(input: &Map<String, Value>) -> Result<()> {
    closed(
        input,
        &[
            "profile",
            "instructions",
            "prompt",
            "inputs",
            "output_schema",
            "permissions",
            "require",
        ],
    )?;
    ensure!(
        input
            .get("profile")
            .is_some_and(|p| p.is_string() || p.is_object()),
        "harness workload requires a runner profile name"
    );
    text(input.get("prompt"), true, MAX_STRING_BYTES)?;
    text(input.get("instructions"), false, MAX_STRING_BYTES)?;
    if let Some(inputs) = input.get("inputs") {
        object(inputs)?;
    }
    if let Some(mode) = input.get("permissions") {
        ensure!(
            mode.as_str()
                .is_some_and(|m| crate::harness::PERMISSION_MODES.contains(&m)),
            "harness permissions must be read-only or workspace-write"
        );
    }
    if let Some(required) = input.get("require") {
        let required = required
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("require must be an array"))?;
        ensure!(
            required.len() <= 8
                && required.iter().all(|c| c
                    .as_str()
                    .is_some_and(|c| crate::harness::CAPABILITIES.contains(&c))),
            "unknown required harness capability"
        );
    }
    let schema = input
        .get("output_schema")
        .ok_or_else(|| anyhow::anyhow!("harness workload requires an output schema"))?;
    object(schema)?;
    local_schema(schema, 0)?;
    jsonschema::validator_for(schema)
        .map_err(|_| anyhow::anyhow!("invalid harness output schema"))?;
    Ok(())
}

fn validate_agent(input: &Map<String, Value>) -> Result<()> {
    closed(
        input,
        &[
            "model",
            "instructions",
            "prompt",
            "params",
            "tools",
            "max_rounds",
            "max_tool_calls",
            "output_schema",
        ],
    )?;
    ensure!(
        input
            .get("model")
            .and_then(Value::as_str)
            .is_some_and(|model| name(model, 128, true)),
        "agent requires a static configured model alias"
    );
    text(input.get("prompt"), true, MAX_STRING_BYTES)?;
    text(input.get("instructions"), false, MAX_STRING_BYTES)?;
    if let Some(params) = input.get("params") {
        object(params)?;
    }
    let empty = BTreeSet::new();
    validate_references(&input["prompt"], &empty)?;
    if let Some(instructions) = input.get("instructions") {
        validate_references(instructions, &empty)?;
    }
    bounded_integer(input.get("max_rounds"), MAX_ROUNDS)?;
    bounded_integer(input.get("max_tool_calls"), MAX_TOOL_CALLS)?;
    if let Some(tools) = input.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("agent tools must be an array"))?;
        ensure!(tools.len() <= MAX_TOOL_CALLS, "too many agent tools");
        let mut names = BTreeSet::new();
        for tool in tools {
            let tool = object(tool)?;
            closed(tool, &["name", "type", "description"])?;
            let tool_name = tool
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("agent tool name required"))?;
            ensure!(
                name(tool_name, 64, false) && names.insert(tool_name),
                "agent tool names must be valid and unique"
            );
            let kind = tool
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("agent tool type required"))?;
            ensure!(
                kind.starts_with("plugin.") && kind.len() > 7 && name(kind, 128, true),
                "agent tools require explicit configured plugin types"
            );
            text(tool.get("description"), false, 4096)?;
        }
    }
    if let Some(schema) = input.get("output_schema") {
        object(schema)?;
        local_schema(schema, 0)?;
        jsonschema::validator_for(schema)
            .map_err(|_| anyhow::anyhow!("invalid agent output schema"))?;
    }
    Ok(())
}

fn local_schema(value: &Value, depth: usize) -> Result<()> {
    ensure!(
        depth <= MAX_JSON_DEPTH,
        "output schema nesting limit exceeded"
    );
    match value {
        Value::Object(map) => {
            // Output validation must never become an implicit HTTP request.
            ensure!(
                !map.contains_key("$id"),
                "output schemas cannot change their reference base"
            );
            for key in ["$ref", "$dynamicRef", "$recursiveRef"] {
                if let Some(reference) = map.get(key) {
                    ensure!(
                        reference
                            .as_str()
                            .is_some_and(|value| value.starts_with('#')),
                        "output schema references must stay local"
                    );
                }
            }
            if let Some(draft) = map.get("$schema") {
                ensure!(
                    draft.as_str().is_some_and(|v| matches!(
                        v,
                        "http://json-schema.org/draft-07/schema#"
                            | "https://json-schema.org/draft-07/schema#"
                            | "https://json-schema.org/draft/2019-09/schema"
                            | "https://json-schema.org/draft/2020-12/schema"
                    )),
                    "unsupported output schema dialect"
                );
            }
            for key in [
                "$defs",
                "definitions",
                "properties",
                "patternProperties",
                "dependentSchemas",
            ] {
                if let Some(Value::Object(children)) = map.get(key) {
                    for child in children.values() {
                        local_schema(child, depth + 1)?;
                    }
                }
            }
            for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
                if let Some(Value::Array(children)) = map.get(key) {
                    for child in children {
                        local_schema(child, depth + 1)?;
                    }
                }
            }
            for key in [
                "additionalProperties",
                "unevaluatedProperties",
                "additionalItems",
                "propertyNames",
                "not",
                "if",
                "then",
                "else",
                "contains",
                "unevaluatedItems",
                "contentSchema",
                "items",
            ] {
                if let Some(child) = map.get(key) {
                    local_schema(child, depth + 1)?;
                }
            }
            if let Some(Value::Object(children)) = map.get("dependencies") {
                for child in children.values().filter(|child| child.is_object()) {
                    local_schema(child, depth + 1)?;
                }
            }
        }
        Value::Array(items) => {
            for child in items {
                local_schema(child, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn segments(pointer: &str) -> Result<Vec<String>> {
    ensure!(
        pointer.starts_with('/') && pointer.len() <= 1024,
        "invalid flow reference pointer"
    );
    pointer[1..]
        .split('/')
        .map(|segment| {
            let mut output = String::new();
            let mut chars = segment.chars();
            while let Some(character) = chars.next() {
                if character == '~' {
                    output.push(match chars.next() {
                        Some('0') => '~',
                        Some('1') => '/',
                        _ => bail!("invalid JSON pointer escape"),
                    });
                } else {
                    output.push(character);
                }
            }
            Ok(output)
        })
        .collect()
}

fn check_reference(pointer: &str, allowed: Option<&BTreeSet<String>>) -> Result<()> {
    let parts = segments(pointer)?;
    match parts.first().map(String::as_str) {
        Some("params") => Ok(()),
        Some("steps") if parts.len() >= 3 && parts[2] == "output" => {
            ensure!(name(&parts[1], 64, true), "invalid step output reference");
            if let Some(allowed) = allowed {
                ensure!(
                    allowed.contains(&parts[1]),
                    "step output reference requires a declared earlier dependency"
                );
            }
            Ok(())
        }
        _ => bail!("references must name params or a step output"),
    }
}

fn templates(value: &str) -> Result<Vec<(usize, usize, &str)>> {
    let mut found = Vec::new();
    let mut offset = 0;
    while let Some(start) = value[offset..].find("${") {
        let start = offset + start;
        let tail = &value[start + 2..];
        let length = tail
            .find('}')
            .ok_or_else(|| anyhow::anyhow!("unterminated flow template"))?;
        let end = start + 2 + length + 1;
        found.push((start, end, &tail[..length]));
        offset = end;
    }
    Ok(found)
}

pub fn validate_references(value: &Value, allowed: &BTreeSet<String>) -> Result<()> {
    fn walk(value: &Value, allowed: &BTreeSet<String>, depth: usize) -> Result<()> {
        ensure!(depth <= MAX_JSON_DEPTH, "flow value nesting limit exceeded");
        match value {
            Value::Object(map) => {
                if let Some(reference) = map.get("$ref") {
                    ensure!(map.len() == 1, "typed flow references contain only $ref");
                    check_reference(
                        reference
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("flow reference must be a string"))?,
                        Some(allowed),
                    )?;
                } else {
                    for child in map.values() {
                        walk(child, allowed, depth + 1)?;
                    }
                }
            }
            Value::Array(items) => {
                for child in items {
                    walk(child, allowed, depth + 1)?;
                }
            }
            Value::String(text) => {
                ensure!(
                    text.len() <= MAX_STRING_BYTES,
                    "flow text exceeds size limit"
                );
                for (_, _, pointer) in templates(text)? {
                    check_reference(pointer, Some(allowed))?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    walk(value, allowed, 0)
}

/// Resolve references in authored input only. Model-selected tool arguments
/// and referenced values are literal data and must not be rendered again.
pub fn render(value: &Value, context: &Value) -> Result<Value> {
    ensure!(
        serde_json::to_vec(context)?.len() <= MAX_CONTEXT_BYTES,
        "flow context exceeds size limit"
    );
    ensure!(
        serde_json::to_vec(value)?.len() <= MAX_CONTEXT_BYTES,
        "authored value exceeds size limit"
    );
    fn charge(remaining: &mut usize, bytes: usize) -> Result<()> {
        *remaining = remaining
            .checked_sub(bytes)
            .ok_or_else(|| anyhow::anyhow!("rendered input exceeds size limit"))?;
        Ok(())
    }
    fn resolve<'a>(pointer: &str, context: &'a Value) -> Result<&'a Value> {
        check_reference(pointer, None)?;
        context
            .pointer(pointer)
            .ok_or_else(|| anyhow::anyhow!("flow reference target is missing"))
    }
    fn walk(value: &Value, context: &Value, depth: usize, remaining: &mut usize) -> Result<Value> {
        ensure!(depth <= MAX_JSON_DEPTH, "flow value nesting limit exceeded");
        match value {
            Value::Object(map) => {
                if let Some(reference) = map.get("$ref") {
                    ensure!(map.len() == 1, "typed flow references contain only $ref");
                    let referenced = resolve(
                        reference
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("flow reference must be a string"))?,
                        context,
                    )?;
                    // Charge before cloning: repeating a large retained value
                    // cannot allocate the whole expanded graph before refusal.
                    charge(remaining, serde_json::to_vec(referenced)?.len())?;
                    Ok(referenced.clone())
                } else {
                    charge(remaining, 2 + map.len().saturating_sub(1))?;
                    let mut result = Map::new();
                    for (key, child) in map {
                        charge(remaining, serde_json::to_vec(key)?.len() + 1)?;
                        let child = walk(child, context, depth + 1, remaining)?;
                        result.insert(key.clone(), child);
                    }
                    Ok(Value::Object(result))
                }
            }
            Value::Array(items) => {
                charge(remaining, 2 + items.len().saturating_sub(1))?;
                let mut result = Vec::new();
                for child in items {
                    result.push(walk(child, context, depth + 1, remaining)?);
                }
                Ok(Value::Array(result))
            }
            Value::String(value) => {
                ensure!(
                    value.len() <= MAX_STRING_BYTES,
                    "authored text exceeds size limit"
                );
                let mut result = String::new();
                let mut previous = 0;
                for (start, end, pointer) in templates(value)? {
                    let replacement = resolve(pointer, context)?;
                    let replacement = match replacement {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    let length = result
                        .len()
                        .saturating_add(start - previous)
                        .saturating_add(replacement.len());
                    ensure!(
                        length <= MAX_STRING_BYTES,
                        "rendered text exceeds size limit"
                    );
                    result.push_str(&value[previous..start]);
                    result.push_str(&replacement);
                    previous = end;
                }
                ensure!(
                    result.len().saturating_add(value.len() - previous) <= MAX_STRING_BYTES,
                    "rendered text exceeds size limit"
                );
                result.push_str(&value[previous..]);
                charge(remaining, serde_json::to_vec(&result)?.len())?;
                Ok(Value::String(result))
            }
            other => {
                charge(remaining, serde_json::to_vec(other)?.len())?;
                Ok(other.clone())
            }
        }
    }
    let mut remaining = MAX_CONTEXT_BYTES;
    let output = walk(value, context, 0, &mut remaining)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn workflow() -> Value {
        json!({"type":"workflow","input":{"params":{"task":"hello"},"steps":[
        {"id":"a","action":{"type":"echo","input":{"text":{"$ref":"/params/task"}}}},
        {"id":"b","needs":["a"],"action":{"work_id":"reviewer","params":{"task":{"$ref":"/steps/a/output/text"}}}}
    ],"output":{"result":{"$ref":"/steps/b/output"}}}})
    }
    #[test]
    fn dependencies_and_closed_contract_reject_forward_self_and_undeclared_reads() {
        validate(&workflow()).unwrap();
        for needs in [
            json!([]),
            json!(["b"]),
            json!(["missing"]),
            json!(["a", "a"]),
        ] {
            let mut value = workflow();
            value["input"]["steps"][1]["needs"] = needs;
            assert!(validate(&value).is_err());
        }
        let mut value = workflow();
        value["input"]["steps"][1]["id"] = json!("a");
        assert!(validate(&value).is_err());
        let mut value = workflow();
        value["input"]["steps"][0]["command"] = json!("unexpected");
        assert!(validate(&value).is_err());
    }
    #[test]
    fn authored_steps_cannot_select_internal_model_transport() {
        let mut value = workflow();
        value["input"]["steps"][0]["action"] =
            json!({"type":"agent_turn","input":{"model":"local","request":{"messages":[]}}});
        assert!(validate(&value).is_err());
    }

    #[test]
    fn typed_refs_preserve_values_and_untrusted_outputs_are_not_evaluated_twice() {
        let context = json!({"params":{"count":7,"a/b":{"x~y":true}},"steps":{"a":{"output":{"text":"${/params/secret}","data":{"$ref":"/params/secret"}}}}});
        assert_eq!(render(&json!({"n":{"$ref":"/params/count"},"flag":{"$ref":"/params/a~1b/x~0y"},"text":"Count ${/params/count}"}),&context).unwrap(),json!({"n":7,"flag":true,"text":"Count 7"}));
        assert_eq!(
            render(&json!({"$ref":"/steps/a/output/data"}), &context).unwrap(),
            json!({"$ref":"/params/secret"})
        );
        assert_eq!(
            render(&json!("${/steps/a/output/text}"), &context).unwrap(),
            json!("${/params/secret}")
        );
    }
    #[test]
    fn missing_or_malformed_refs_refuse_instead_of_becoming_empty_text() {
        for value in [
            json!({"$ref":"/params/missing"}),
            json!("${/params/x"),
            json!({"$ref":"/params/x","extra":1}),
            json!({"$ref":"/private/token"}),
            json!({"$ref":"/params/~2"}),
        ] {
            assert!(render(&value, &json!({"params":{}})).is_err());
        }
    }
    #[test]
    fn agent_tool_allowlist_names_and_budgets_are_structural_not_grants() {
        let agent = json!({"type":"agent","input":{"model":"local","prompt":"${/params/task}","tools":[{"name":"lookup","type":"plugin.lookup"}],"max_rounds":4,"max_tool_calls":8,"output_schema":{"type":"object"}}});
        validate(&agent).unwrap();
        let mut duplicate = agent.clone();
        duplicate["input"]["tools"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"lookup","type":"plugin.other"}));
        assert!(validate(&duplicate).is_err());
        for tool in [
            json!({"name":"shell","type":"command"}),
            json!({"name":"x.y","type":"plugin.x"}),
            json!({"name":"x","type":"plugin.x","command":["sh"]}),
        ] {
            let mut value = agent.clone();
            value["input"]["tools"] = json!([tool]);
            assert!(validate(&value).is_err());
        }
        for budget in [json!(0), json!(9), json!(1.5), json!(-1)] {
            let mut value = agent.clone();
            value["input"]["max_rounds"] = budget;
            assert!(validate(&value).is_err());
        }
    }
    #[test]
    fn output_schema_cannot_fetch_remote_documents_or_change_base() {
        for schema in [
            json!({"$ref":"https://example.invalid/schema"}),
            json!({"$id":"https://example.invalid/base","$ref":"#/x"}),
            json!({"$schema":"https://example.invalid/meta"}),
        ] {
            let value = json!({"type":"agent","input":{"model":"local","prompt":"Task","output_schema":schema}});
            assert!(validate(&value).is_err());
        }
        validate(&json!({"type":"agent","input":{"model":"local","prompt":"Task","output_schema":{"$defs":{"answer":{"type":"integer"}},"type":"object","properties":{"answer":{"$ref":"#/$defs/answer"}}}}})).unwrap();
        validate(&json!({"type":"agent","input":{"model":"local","prompt":"Task","output_schema":{"type":"object","properties":{"$id":{"type":"string"},"$ref":{"type":"string"}},"default":{"$ref":"https://example.invalid/literal-data"}}}})).unwrap();
    }
    #[test]
    fn child_agent_params_bind_parent_outputs_without_leaking_parent_step_scope() {
        let mut value = workflow();
        value["input"]["steps"][1]["action"] = json!({"type":"agent","input":{"model":"local","prompt":"Inspect ${/params/task}","params":{"task":{"$ref":"/steps/a/output/text"}}}});
        validate(&value).unwrap();
        value["input"]["steps"][1]["action"]["input"]["prompt"] = json!("${/steps/a/output/text}");
        assert!(validate(&value).is_err());
    }
    #[test]
    fn repeated_large_typed_references_refuse_before_expansion_allocation() {
        let context = json!({"params":{"large":"x".repeat(512 * 1024)}});
        let many = Value::Array(vec![json!({"$ref":"/params/large"}); 10000]);
        assert!(render(&many, &context).is_err());
        assert_eq!(
            render(
                &json!({"quoted":"a\\b\"c","list":[true,1,null]}),
                &json!({"params":{}})
            )
            .unwrap(),
            json!({"quoted":"a\\b\"c","list":[true,1,null]})
        );
    }

    #[test]
    fn render_expansion_is_bounded() {
        assert!(
            render(
                &json!("${/params/large}${/params/large}"),
                &json!({"params":{"large":"x".repeat(MAX_STRING_BYTES)}})
            )
            .is_err()
        );
        let mut value = workflow();
        value["input"]["steps"] = json!([]);
        assert!(validate(&value).is_err());
    }
}

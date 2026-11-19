//! Bounded default projections; exact selected records use the same scoped API.
use serde_json::{Value, json};
pub(crate) fn preview(value: &Value) -> Value {
    let text = value.to_string();
    if text.len() <= 2048 {
        return value.clone();
    }
    let prefix: String = text.chars().take(512).collect();
    json!({"summary":true,"bytes":text.len(),"preview":prefix})
}
pub(crate) fn run_summary(run: Value) -> Value {
    json!({"summary":true,"run_id":run["run_id"],"work_id":run["work_id"],"status":run["status"],"created":run["created"],"parent":run["parent"],"output":preview(&run["output"]),"diagnostic":run["diagnostic"],"error":run["error"],"step_count":run["steps"].as_array().map(Vec::len),"effect_count":run["effects"].as_array().map(Vec::len),"effect":preview(&run["effect"])})
}

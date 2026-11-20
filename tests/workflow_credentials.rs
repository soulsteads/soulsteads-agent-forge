use agent_forge::flow_schema::validate;
use serde_json::json;

#[test]
fn credential_check_accepts_only_a_literal_alias_in_a_direct_workflow_step() {
    let action = json!({"type":"credential_check","input":{"credential":"release-check"}});
    let workflow =
        |action| json!({"type":"workflow","input":{"steps":[{"id":"check","action":action}]}});
    assert!(validate(&workflow(action.clone())).is_ok());
    assert!(validate(&action).is_err());
    assert!(validate(&workflow(workflow(action.clone()))).is_err());
    for input in [
        json!({"credential":"${/params/key}"}),
        json!({"credential":{"$ref":"/params/key"}}),
        json!({"credential":"release-check","url":"https://example.invalid"}),
        json!({"credential":".."}),
    ] {
        assert!(validate(&workflow(json!({"type":"credential_check","input":input}))).is_err());
    }
}

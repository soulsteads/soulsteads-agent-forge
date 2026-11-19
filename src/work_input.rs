//! Public registration diagnostics: schema locations and expectations, never values.
use anyhow::Result;
use jsonschema::error::{TypeKind, ValidationErrorKind};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
#[error("work input does not match supported schema: {0}")]
pub(crate) struct InvalidWorkInput(String);

const MAX_ISSUES: usize = 5;
const MAX_ISSUE_BYTES: usize = 384;

fn bounded(mut text: String) -> String {
    if text.len() > MAX_ISSUE_BYTES {
        let mut end = MAX_ISSUE_BYTES - 3;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("...");
    }
    text
}

fn pointer(path: &str, property: Option<&str>) -> String {
    let mut path = format!("/input{path}");
    if let Some(property) = property {
        path.push('/');
        path.push_str(&property.replace('~', "~0").replace('/', "~1"));
    }
    // Quoting keeps control characters in property names out of public messages.
    serde_json::to_string(&path).expect("string serializes")
}

pub(crate) fn validate(schema: &Value, input: &Value) -> Result<()> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|_| crate::core::Invalid("unsupported work input schema"))?;
    let mut issues = Vec::new();
    for error in validator.iter_errors(input) {
        let path = error.instance_path().to_string();
        match error.kind() {
            ValidationErrorKind::AdditionalProperties { unexpected } => {
                for property in unexpected {
                    issues.push(bounded(format!(
                        "unexpected property {}",
                        pointer(&path, Some(property))
                    )));
                    if issues.len() > MAX_ISSUES {
                        break;
                    }
                }
            }
            ValidationErrorKind::Required { property } => issues.push(bounded(format!(
                "missing required property {}",
                pointer(&path, property.as_str())
            ))),
            ValidationErrorKind::Type { kind } => {
                let expected = match kind {
                    TypeKind::Single(kind) => kind.to_string(),
                    TypeKind::Multiple(kinds) => kinds
                        .iter()
                        .map(|kind| kind.to_string())
                        .collect::<Vec<_>>()
                        .join(" or "),
                };
                issues.push(bounded(format!(
                    "expected {expected} at {}",
                    pointer(&path, None)
                )));
            }
            kind => issues.push(bounded(format!(
                "{} violates {}; see the supported type schema",
                pointer(&path, None),
                kind.keyword()
            ))),
        }
        if issues.len() > MAX_ISSUES {
            break;
        }
    }
    if issues.is_empty() {
        return Ok(());
    }
    if issues.len() > MAX_ISSUES {
        issues.truncate(MAX_ISSUES);
        issues.push("further issues omitted".into());
    }
    Err(InvalidWorkInput(issues.join("; ")).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn registration_schema_diagnostics_are_bounded_and_only_describe_locations() {
        // Match work schemas with declared properties; an empty property set
        // uses jsonschema's FalseSchema optimization instead.
        let schema = json!({"type":"object","properties":{"allowed":{"type":"string"}},"additionalProperties":false});
        let escaped = validate(&schema, &json!({"a/b~c\n":"PRIVATE_VALUE"}))
            .unwrap_err()
            .to_string();
        assert!(escaped.contains(r"/input/a~1b~0c\n"), "{escaped}");
        assert!(!escaped.contains("PRIVATE_VALUE"));
        assert!(!escaped.chars().any(char::is_control));
        let input: serde_json::Map<String, Value> = (0..20)
            .map(|n| {
                (
                    format!("{n:02}{}", "界".repeat(500)),
                    json!("PRIVATE_VALUE"),
                )
            })
            .collect();
        let message = validate(&schema, &Value::Object(input))
            .unwrap_err()
            .to_string();
        assert_eq!(message.matches("unexpected property").count(), MAX_ISSUES);
        assert!(message.ends_with("further issues omitted"));
        assert!(message.len() < 2048);
        assert!(!message.contains("PRIVATE_VALUE"));
    }
}

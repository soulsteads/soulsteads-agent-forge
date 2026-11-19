//! The one canonical JSON encoding used for every digest, immutable-binding
//! comparison and persisted identity string of a [`Value`].
//!
//! Object keys are emitted in sorted (byte) order, recursively, with the compact
//! `serde_json` scalar encoding. The result is independent of whether any crate in
//! the build enables `serde_json/preserve_order` (the ACP dependencies do, and
//! cannot be built without it), and it equals the bytes `serde_json::to_vec`
//! produced for a `Value` before that feature was unified, so digests recorded by
//! earlier builds keep matching. Typed structs are not affected by map ordering:
//! serialize those with `serde_json` directly and keep their declared field order.
use serde::{Serialize, Serializer, ser::SerializeMap, ser::SerializeSeq};
use serde_json::Value;

struct Sorted<'a>(&'a Value);
impl Serialize for Sorted<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort_unstable();
                let mut out = serializer.serialize_map(Some(keys.len()))?;
                for key in keys {
                    out.serialize_entry(key, &Sorted(&map[key]))?;
                }
                out.end()
            }
            Value::Array(items) => {
                let mut out = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    out.serialize_element(&Sorted(item))?;
                }
                out.end()
            }
            scalar => scalar.serialize(serializer),
        }
    }
}

/// `serialize_with` for an optional `Value` field of a typed struct whose JSON text
/// is compared or digested: the struct keeps its declared field order, the value
/// inside is sorted.
pub fn serialize_option<S: Serializer>(
    value: &Option<Value>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => serializer.serialize_some(&Sorted(value)),
        None => serializer.serialize_none(),
    }
}

/// Canonical bytes of `value` (sorted keys, compact).
pub fn to_vec(value: &Value) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&Sorted(value))
}
/// Canonical text of `value` (sorted keys, compact).
pub fn to_string(value: &Value) -> serde_json::Result<String> {
    serde_json::to_string(&Sorted(value))
}
/// Canonical bytes of anything serializable that is built from JSON-like data
/// (maps become sorted objects); use only where a `Value` was hashed before.
pub fn to_vec_of<T: Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    to_vec(&serde_json::to_value(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_are_sorted_recursively_whatever_the_insertion_order() {
        let value = json!({"z":1,"a":{"y":[{"b":2,"a":1}],"b":null},"m":"x"});
        assert_eq!(
            to_string(&value).unwrap(),
            r#"{"a":{"b":null,"y":[{"a":1,"b":2}]},"m":"x","z":1}"#
        );
        assert_eq!(
            to_vec(&value).unwrap(),
            to_string(&value).unwrap().into_bytes()
        );
    }

    #[test]
    fn scalar_encoding_matches_serde_json() {
        let value = json!({"s":"é\n\"\u{1}","n":-12,"f":1.5,"t":true,"e":[],"o":{}});
        let sorted: Value = serde_json::from_str(&to_string(&value).unwrap()).unwrap();
        assert_eq!(sorted, value);
        assert_eq!(
            to_string(&json!({"s":"é\n\"\u{1}","f":1.5})).unwrap(),
            r#"{"f":1.5,"s":"é\n\"\u0001"}"#
        );
    }
}

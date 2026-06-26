//! Full-fidelity *key structure* of a tool's return value, for strict pre-flight
//! validation of field access in agent-written code.
//!
//! This is the validation-tier counterpart to [`super::shape`]. Where `shape`
//! renders a *lossy, size-bounded* exemplar for the model to read in the
//! `execute_python` description (token-cheap discovery), a `KeySet` retains the
//! **complete, uncapped** set of keys at every depth so the AST validator can
//! reject a wrong field access (`result["lgoin"]`) *before* execution without
//! risking false positives.
//!
//! Two design points make strict checking safe:
//!
//! 1. **No size caps.** Unlike `shape` (MAX_FIELDS/MAX_DEPTH/MAX_LEN), a `KeySet`
//!    keeps every key. A real key is never missing, so "key not in set" is a
//!    trustworthy signal. This never reaches the model, so the bloat the lossy
//!    shape avoids is irrelevant here.
//! 2. **Union across observations.** Tools whose result varies by entity (e.g.
//!    GitHub User vs Organization) are learned by *merging* every observed value,
//!    so a key present in any variant is accepted.
//!
//! A `KeySet` is serializable so the gateway can ship the learned structure to
//! the Python worker (which runs the validator) over the control channel.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The key structure of a value, retained at full fidelity for validation.
///
/// - `Object` carries each field name mapped to the key structure of its value.
/// - `Array` carries the merged key structure across all elements (so an array
///   of objects exposes the union of element fields).
/// - `Leaf` is any scalar / null / opaque value: nothing more to descend into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "k", rename_all = "lowercase")]
pub enum KeySet {
    Object {
        #[serde(default)]
        fields: BTreeMap<String, KeySet>,
    },
    Array {
        items: Box<KeySet>,
    },
    #[default]
    Leaf,
}

impl KeySet {
    /// Extract the full nested key structure from a real result value.
    pub fn extract(value: &Value) -> KeySet {
        match value {
            Value::Object(map) => {
                let mut fields = BTreeMap::new();
                for (k, v) in map {
                    fields.insert(k.clone(), KeySet::extract(v));
                }
                KeySet::Object { fields }
            }
            Value::Array(items) => {
                // Merge every element so the array's KeySet is the union of all
                // element structures (variant elements don't lose keys).
                let mut acc = KeySet::Leaf;
                for it in items {
                    acc = acc.merge(KeySet::extract(it));
                }
                KeySet::Array {
                    items: Box::new(acc),
                }
            }
            _ => KeySet::Leaf,
        }
    }

    /// Build a `KeySet` from a declared JSON-Schema `outputSchema`, so the first
    /// call to a tool can be validated before any value has been observed.
    ///
    /// Walks `properties` (objects) and `items` (arrays). `$ref`/`oneOf`/`allOf`
    /// and other constructs we can't resolve cheaply collapse to `Leaf` — the
    /// validator simply won't descend there (conservative: no false positives).
    pub fn from_output_schema(schema: &Value) -> KeySet {
        let Some(obj) = schema.as_object() else {
            return KeySet::Leaf;
        };
        // A schema node's structure is driven by its declared `type`, but `type`
        // may be absent; infer from `properties`/`items` when so.
        let ty = obj.get("type").and_then(Value::as_str);
        if ty == Some("object") || obj.contains_key("properties") {
            let mut fields = BTreeMap::new();
            if let Some(Value::Object(props)) = obj.get("properties") {
                for (k, v) in props {
                    fields.insert(k.clone(), KeySet::from_output_schema(v));
                }
            }
            return KeySet::Object { fields };
        }
        if ty == Some("array") || obj.contains_key("items") {
            let items = match obj.get("items") {
                Some(it) => KeySet::from_output_schema(it),
                None => KeySet::Leaf,
            };
            return KeySet::Array {
                items: Box::new(items),
            };
        }
        KeySet::Leaf
    }

    /// Union two key structures. Used to accumulate keys across multiple calls to
    /// the same tool (entity variants) and to merge array elements.
    ///
    /// Object ∪ Object = union of fields (recursively merged on shared keys).
    /// Array ∪ Array  = array of merged element structures.
    /// Anything mismatched (Object vs Array, or either vs Leaf) widens to the
    /// *more structured* side so previously-learned keys are never dropped.
    pub fn merge(self, other: KeySet) -> KeySet {
        match (self, other) {
            (KeySet::Object { fields: mut a }, KeySet::Object { fields: b }) => {
                for (k, v) in b {
                    match a.remove(&k) {
                        Some(existing) => {
                            a.insert(k, existing.merge(v));
                        }
                        None => {
                            a.insert(k, v);
                        }
                    }
                }
                KeySet::Object { fields: a }
            }
            (KeySet::Array { items: a }, KeySet::Array { items: b }) => KeySet::Array {
                items: Box::new(a.merge(*b)),
            },
            // Mismatched or leaf: keep whichever side carries structure.
            (KeySet::Leaf, other) => other,
            (this, KeySet::Leaf) => this,
            // Object vs Array (genuinely polymorphic return): prefer Object, the
            // form the model most commonly indexes by literal key. Rare; safe.
            (obj @ KeySet::Object { .. }, KeySet::Array { .. }) => obj,
            (KeySet::Array { .. }, obj @ KeySet::Object { .. }) => obj,
        }
    }

    /// True if this node has no useful structure to validate against.
    pub fn is_empty(&self) -> bool {
        match self {
            KeySet::Object { fields } => fields.is_empty(),
            KeySet::Array { items } => items.is_empty(),
            KeySet::Leaf => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_flat_object() {
        let ks = KeySet::extract(&json!({"id": 1, "login": "x"}));
        match ks {
            KeySet::Object { fields } => {
                assert!(fields.contains_key("id"));
                assert!(fields.contains_key("login"));
                assert_eq!(fields["id"], KeySet::Leaf);
            }
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn extract_nested() {
        let ks = KeySet::extract(&json!({"user": {"login": "x", "id": 1}}));
        let KeySet::Object { fields } = ks else {
            panic!("object")
        };
        let KeySet::Object { fields: user } = &fields["user"] else {
            panic!("nested object")
        };
        assert!(user.contains_key("login"));
        assert!(user.contains_key("id"));
    }

    #[test]
    fn array_unions_element_fields() {
        // Variant elements: first has `a`, second has `b`. Union must keep both.
        let ks = KeySet::extract(&json!([{"a": 1}, {"b": 2}]));
        let KeySet::Array { items } = ks else {
            panic!("array")
        };
        let KeySet::Object { fields } = *items else {
            panic!("array of objects")
        };
        assert!(fields.contains_key("a"));
        assert!(fields.contains_key("b"));
    }

    #[test]
    fn merge_unions_variant_objects() {
        // GitHub User vs Org: different keys; union accepts either.
        let user = KeySet::extract(&json!({"login": "x", "name": "n"}));
        let org = KeySet::extract(&json!({"login": "o", "company": "c"}));
        let KeySet::Object { fields } = user.merge(org) else {
            panic!("object")
        };
        assert!(fields.contains_key("login"));
        assert!(fields.contains_key("name"));
        assert!(fields.contains_key("company"));
    }

    #[test]
    fn merge_recurses_into_shared_keys() {
        let a = KeySet::extract(&json!({"user": {"login": "x"}}));
        let b = KeySet::extract(&json!({"user": {"id": 1}}));
        let KeySet::Object { fields } = a.merge(b) else {
            panic!("object")
        };
        let KeySet::Object { fields: user } = &fields["user"] else {
            panic!("nested")
        };
        assert!(user.contains_key("login"));
        assert!(user.contains_key("id"));
    }

    #[test]
    fn merge_leaf_keeps_structure() {
        let structured = KeySet::extract(&json!({"a": 1}));
        assert_eq!(
            KeySet::Leaf.merge(structured.clone()),
            structured.clone().merge(KeySet::Leaf)
        );
    }

    #[test]
    fn scalars_and_empty_are_leaf_or_empty() {
        assert_eq!(KeySet::extract(&json!(42)), KeySet::Leaf);
        assert_eq!(KeySet::extract(&json!("hi")), KeySet::Leaf);
        assert!(KeySet::extract(&json!({})).is_empty());
        assert!(KeySet::extract(&json!([])).is_empty());
    }

    #[test]
    fn from_output_schema_object_and_array() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"name": {"type": "string"}}
                    }
                },
                "total": {"type": "integer"}
            }
        });
        let ks = KeySet::from_output_schema(&schema);
        let KeySet::Object { fields } = ks else {
            panic!("object")
        };
        assert!(fields.contains_key("total"));
        let KeySet::Array { items } = &fields["items"] else {
            panic!("array")
        };
        let KeySet::Object { fields: el } = &**items else {
            panic!("array of objects")
        };
        assert!(el.contains_key("name"));
    }

    #[test]
    fn roundtrips_through_json() {
        let ks = KeySet::extract(&json!({"user": {"login": "x"}, "tags": [{"n": 1}]}));
        let s = serde_json::to_string(&ks).unwrap();
        let back: KeySet = serde_json::from_str(&s).unwrap();
        assert_eq!(ks, back);
    }
}

//! Infer a compact, size-bounded *shape* from a real tool-result value.
//!
//! Code-mode's win — one model turn, one tool call — leaks when the model has to
//! *guess* the structure of a tool's return value (e.g. `issue["linked_pr"]`)
//! and gets a `KeyError`, costing a retry. Declared `outputSchema`s are usually
//! absent and, when present, far too verbose to put in front of the model on
//! every turn.
//!
//! Instead we learn the shape from the *actual* post-unwrap value the worker
//! returns and render a tiny exemplar of it: field names + leaf type names,
//! depth-capped, fields-capped, and arrays collapsed to a single element. This
//! is a projection (like the input signature already is), **not** a schema — no
//! descriptions, constraints, enums, or examples. The result is a handful of
//! characters that kill the single biggest source of shape-guessing retries.
//!
//! Critically, the output is bounded *by construction*: even a 5,000-field,
//! deeply nested response renders to a small, fixed-size string. This is what
//! keeps the feature from re-importing the schema bloat it exists to avoid.

use serde_json::Value;

/// Maximum nesting depth rendered. Beyond this, values collapse to their type
/// name (`{...}` / `[...]`). Set to 3 so the common GitHub shape — a top-level
/// object whose field is a list of objects (`labels: [{name, color}]`) — still
/// shows one level of element fields, while genuinely deep trees collapse.
const MAX_DEPTH: usize = 3;

/// Maximum number of object keys rendered at any level. Extra keys are elided
/// with a trailing `...`.
const MAX_FIELDS: usize = 12;

/// Hard cap on the rendered shape length (characters). A safety net so a
/// pathological value can never bloat the description; truncated with `…`.
const MAX_LEN: usize = 600;

/// Infer a one-line, size-bounded shape string from a real result value.
///
/// Returns `None` when there is nothing useful to show (e.g. a bare scalar or
/// `null`, where the signature's return type already conveys everything).
pub fn infer(value: &Value) -> Option<String> {
    // Only object/array values carry structure worth teaching. A scalar return
    // is already described by the function's return-type hint.
    match value {
        Value::Object(_) | Value::Array(_) => {}
        _ => return None,
    }

    let mut s = render(value, 0);
    if s.len() > MAX_LEN {
        // Truncate on a char boundary and mark it.
        let mut end = MAX_LEN;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    Some(s)
}

/// Render `value` at the given depth into a compact shape exemplar.
fn render(value: &Value, depth: usize) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::Number(n) => {
            if n.is_f64() && !n.is_i64() && !n.is_u64() {
                "float".to_string()
            } else {
                "int".to_string()
            }
        }
        Value::String(_) => "str".to_string(),
        Value::Array(items) => render_array(items, depth),
        Value::Object(map) => render_object(map, depth),
    }
}

fn render_array(items: &[Value], depth: usize) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }
    if depth >= MAX_DEPTH {
        return "[...]".to_string();
    }
    // Collapse to a single representative element: prefer the richest element
    // (an object) so the exemplar shows field names, not just `[str]`.
    let exemplar = items
        .iter()
        .find(|v| matches!(v, Value::Object(_)))
        .unwrap_or(&items[0]);
    format!("[{}]", render(exemplar, depth + 1))
}

fn render_object(map: &serde_json::Map<String, Value>, depth: usize) -> String {
    if map.is_empty() {
        return "{}".to_string();
    }
    if depth >= MAX_DEPTH {
        return "{...}".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    for (i, (k, v)) in map.iter().enumerate() {
        if i >= MAX_FIELDS {
            parts.push("...".to_string());
            break;
        }
        parts.push(format!("{k}: {}", render(v, depth + 1)));
    }
    format!("{{{}}}", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalars_return_none() {
        assert_eq!(infer(&json!(42)), None);
        assert_eq!(infer(&json!("hi")), None);
        assert_eq!(infer(&json!(true)), None);
        assert_eq!(infer(&json!(null)), None);
    }

    #[test]
    fn flat_object_shape() {
        let v = json!({"id": 7, "title": "x", "open": true, "ratio": 1.5});
        let s = infer(&v).unwrap();
        assert!(s.contains("id: int"), "{s}");
        assert!(s.contains("title: str"), "{s}");
        assert!(s.contains("open: bool"), "{s}");
        assert!(s.contains("ratio: float"), "{s}");
    }

    #[test]
    fn array_collapses_to_one_exemplar() {
        let v = json!([{"a": 1}, {"a": 2}, {"a": 3}]);
        let s = infer(&v).unwrap();
        assert_eq!(s, "[{a: int}]");
    }

    #[test]
    fn array_prefers_object_exemplar() {
        // A mixed list should still surface field names from the object element.
        let v = json!(["x", {"name": "y"}]);
        let s = infer(&v).unwrap();
        assert_eq!(s, "[{name: str}]");
    }

    #[test]
    fn nesting_is_depth_capped() {
        let v = json!({"a": {"b": {"c": {"d": 1}}}});
        let s = infer(&v).unwrap();
        // depth 0 = outer, 1 = a, 2 = b, 3 collapses c's object.
        assert_eq!(s, "{a: {b: {c: {...}}}}");
    }

    #[test]
    fn fields_are_capped() {
        let mut m = serde_json::Map::new();
        for i in 0..50 {
            m.insert(format!("k{i:02}"), json!(i));
        }
        let s = infer(&Value::Object(m)).unwrap();
        assert!(s.contains("..."), "{s}");
        // Only MAX_FIELDS keys + the ellipsis marker.
        let commas = s.matches(", ").count();
        assert!(commas <= MAX_FIELDS, "too many fields rendered: {s}");
    }

    #[test]
    fn length_is_bounded() {
        // A wide, deep structure must never exceed MAX_LEN (+ the ellipsis char).
        let mut m = serde_json::Map::new();
        for i in 0..200 {
            m.insert(
                format!("field_with_a_longish_name_{i}"),
                json!({"nested": "value", "more": [1, 2, 3]}),
            );
        }
        let s = infer(&Value::Object(m)).unwrap();
        assert!(s.chars().count() <= MAX_LEN + 1, "len={}", s.chars().count());
    }

    #[test]
    fn empty_containers() {
        assert_eq!(infer(&json!({})).unwrap(), "{}");
        assert_eq!(infer(&json!([])).unwrap(), "[]");
    }

    #[test]
    fn realistic_github_issue() {
        let v = json!({
            "number": 42,
            "title": "Bug",
            "state": "open",
            "user": {"login": "octocat", "id": 1},
            "labels": [{"name": "bug", "color": "red"}],
        });
        let s = infer(&v).unwrap();
        assert!(s.contains("number: int"), "{s}");
        // serde_json's Map (without preserve_order) renders keys sorted.
        assert!(s.contains("user: {id: int, login: str}"), "{s}");
        assert!(s.contains("labels: [{color: str, name: str}]"), "{s}");
    }
}

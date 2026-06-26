//! Convert MCP tool JSON Schemas into typed Python function definitions.
//!
//! Each upstream tool becomes one Python function `def <fn_name>(...) -> <ret>:`
//! whose body proxies into the gateway via the control channel. We also emit the
//! signature line (for the prompt) and an importable `sdk.py`.

use serde_json::Value;

/// A generated Python binding for a single upstream tool.
#[derive(Debug, Clone)]
pub struct PyBinding {
    /// Python function name, e.g. `everything_get_sum`.
    pub fn_name: String,
    /// Upstream server name (for routing).
    pub server: String,
    /// Original (possibly hyphenated) tool name on the upstream.
    pub tool_name: String,
    /// One-line summary.
    pub summary: String,
    /// `def fn_name(a: int, b: int) -> dict:` (no body).
    pub signature: String,
    /// Ordered parameter names as they appear in the MCP schema (for arg mapping).
    pub params: Vec<ParamSpec>,
    /// Key structure derived from the tool's declared `outputSchema`, if any.
    /// Used to seed return-field validation before any value is observed.
    pub output_keyset: Option<crate::sdk::keyset::KeySet>,
}

#[derive(Debug, Clone)]
pub struct ParamSpec {
    /// Original MCP property name (may be camelCase).
    pub mcp_name: String,
    /// Python parameter name (sanitized).
    pub py_name: String,
    pub required: bool,
}

/// Sanitize an arbitrary MCP name into a valid Python identifier.
pub fn sanitize_ident(raw: &str) -> String {
    let mut s = String::with_capacity(raw.len());
    for (i, ch) in raw.chars().enumerate() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if i == 0 && ch.is_ascii_digit() {
                s.push('_');
            }
            s.push(ch);
        } else {
            s.push('_');
        }
    }
    if s.is_empty() {
        s.push('_');
    }
    if PY_KEYWORDS.contains(&s.as_str()) {
        s.push('_');
    }
    s
}

const PY_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield", "match", "case",
];

/// Build the fully-qualified Python function name `{server}_{tool}`.
pub fn fn_name(server: &str, tool: &str) -> String {
    format!("{}_{}", sanitize_ident(server), sanitize_ident(tool))
}

/// Map a JSON Schema fragment to a Python type-hint string.
pub fn py_type(schema: &Value) -> String {
    // Handle anyOf / oneOf as a Union.
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(variants)) = schema.get(key) {
            let mut parts: Vec<String> = variants.iter().map(py_type).collect();
            parts.dedup();
            if parts.is_empty() {
                return "Any".to_string();
            }
            return parts.join(" | ");
        }
    }

    // enum of strings -> Literal[...]
    if let Some(Value::Array(values)) = schema.get("enum") {
        let lits: Vec<String> = values
            .iter()
            .map(|v| match v {
                Value::String(s) => format!("{:?}", s),
                other => other.to_string(),
            })
            .collect();
        if !lits.is_empty() {
            return format!("Literal[{}]", lits.join(", "));
        }
    }

    let ty = schema.get("type");
    match ty {
        Some(Value::String(t)) => map_simple_type(t, schema),
        Some(Value::Array(types)) => {
            // ["string","null"] -> Optional-like union
            let mut parts: Vec<String> = types
                .iter()
                .filter_map(|v| v.as_str())
                .map(|t| {
                    if t == "null" {
                        "None".to_string()
                    } else {
                        map_simple_type(t, schema)
                    }
                })
                .collect();
            parts.dedup();
            if parts.is_empty() {
                "Any".to_string()
            } else {
                parts.join(" | ")
            }
        }
        _ => "Any".to_string(),
    }
}

fn map_simple_type(t: &str, schema: &Value) -> String {
    match t {
        "string" => "str".to_string(),
        "integer" => "int".to_string(),
        "number" => "float".to_string(),
        "boolean" => "bool".to_string(),
        "null" => "None".to_string(),
        "array" => {
            let item = schema
                .get("items")
                .map(py_type)
                .unwrap_or_else(|| "Any".to_string());
            format!("list[{item}]")
        }
        "object" => {
            // typed dict-ish: fall back to dict[str, Any] for generality
            "dict[str, Any]".to_string()
        }
        _ => "Any".to_string(),
    }
}

/// Determine a return type hint from the tool's optional output schema.
pub fn return_type(output_schema: Option<&Value>) -> String {
    match output_schema {
        Some(s) => py_type(s),
        // MCP tool results are content lists; we surface the structured/parsed
        // result as a dict by default.
        None => "dict[str, Any]".to_string(),
    }
}

/// Build a [`PyBinding`] for one tool.
pub fn build_binding(
    server: &str,
    tool_name: &str,
    summary: &str,
    input_schema: &Value,
    output_schema: Option<&Value>,
) -> PyBinding {
    let fn_name = fn_name(server, tool_name);

    let properties = input_schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let required: Vec<String> = input_schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Preserve declared property order; required params first so non-default
    // params never follow defaulted ones (Python syntax rule).
    let mut params: Vec<ParamSpec> = Vec::new();
    let mut sig_required: Vec<String> = Vec::new();
    let mut sig_optional: Vec<String> = Vec::new();

    for (name, prop_schema) in &properties {
        let is_required = required.contains(name);
        let py_name = sanitize_ident(name);
        let ty = py_type(prop_schema);
        if is_required {
            sig_required.push(format!("{py_name}: {ty}"));
        } else {
            sig_optional.push(format!("{py_name}: {ty} | None = None"));
        }
        params.push(ParamSpec {
            mcp_name: name.clone(),
            py_name,
            required: is_required,
        });
    }

    let mut sig_params = sig_required;
    sig_params.extend(sig_optional);

    let ret = return_type(output_schema);
    let signature = format!("def {fn_name}({}) -> {ret}:", sig_params.join(", "));

    // Seed a validation key structure from the declared outputSchema (if any), so
    // the first call to a tool can be field-checked before a value is observed.
    let output_keyset = output_schema.map(crate::sdk::keyset::KeySet::from_output_schema);

    PyBinding {
        fn_name,
        server: server.to_string(),
        tool_name: tool_name.to_string(),
        summary: summary.to_string(),
        signature,
        params,
        output_keyset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitizes_hyphens_and_keywords() {
        assert_eq!(sanitize_ident("get-sum"), "get_sum");
        assert_eq!(sanitize_ident("class"), "class_");
        assert_eq!(sanitize_ident("2fa"), "_2fa");
    }

    #[test]
    fn maps_basic_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": {"type": "integer"},
                "b": {"type": "integer"},
                "label": {"type": "string"}
            },
            "required": ["a", "b"]
        });
        let b = build_binding("everything", "get-sum", "Returns the sum", &schema, None);
        assert_eq!(b.fn_name, "everything_get_sum");
        assert!(b.signature.starts_with("def everything_get_sum("));
        assert!(b.signature.contains("a: int"));
        assert!(b.signature.contains("b: int"));
        assert!(b.signature.contains("label: str | None = None"));
        assert!(b.signature.ends_with("-> dict[str, Any]:"));
    }

    #[test]
    fn enum_becomes_literal() {
        let s = json!({"type": "string", "enum": ["a", "b"]});
        assert_eq!(py_type(&s), r#"Literal["a", "b"]"#);
    }

    #[test]
    fn array_of_strings() {
        let s = json!({"type": "array", "items": {"type": "string"}});
        assert_eq!(py_type(&s), "list[str]");
    }
}

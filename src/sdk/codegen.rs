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
    /// Whether this tool mutates state (write). Requires explicit authorization
    /// (`allow_mutations`) before the worker will execute it.
    pub is_mutation: bool,
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

/// Verb prefixes that strongly indicate a tool mutates state. Matched against
/// the (lowercased) tool name's leading word, split on `_`/`-`/`.`/space.
const MUTATION_VERBS: &[&str] = &[
    "create",
    "update",
    "delete",
    "remove",
    "add",
    "edit",
    "set",
    "put",
    "patch",
    "write",
    "modify",
    "merge",
    "push",
    "run",
    "exec",
    "execute",
    "install",
    "uninstall",
    "apply",
    "scale",
    "restart",
    "start",
    "stop",
    "kill",
    "terminate",
    "destroy",
    "cleanup",
    "clean",
    "transition",
    "assign",
    "move",
    "rename",
    "copy",
    "upload",
    "send",
    "post",
    "publish",
    "deploy",
    "revert",
    "rollback",
    "cancel",
    "approve",
    "reject",
    "close",
    "reopen",
    "enable",
    "disable",
    "grant",
    "revoke",
    "rotate",
    "reset",
    "drop",
    "insert",
    "fork",
    "sync",
    "trigger",
    "invoke",
    "schedule",
    "provision",
];

/// Read words that override a mutation-verb false positive (e.g. `get_status`,
/// `list_runs`). If the leading word is one of these, treat as read-only.
const READ_VERBS: &[&str] = &[
    "get", "list", "read", "search", "fetch", "find", "describe", "show", "view", "query", "check",
    "status", "info", "lookup", "count", "watch", "download", "export", "diff", "inspect",
    "analyze", "resolve",
];

/// Classify whether a tool is a mutation. Priority:
/// 1. explicit MCP annotation `readOnlyHint` (true => read, false => write),
/// 2. an operator override supplied via config (`Some(bool)`),
/// 3. a name-verb heuristic (default read-only when ambiguous).
pub fn classify_mutation(
    tool_name: &str,
    annotations: Option<&Value>,
    config_override: Option<bool>,
) -> bool {
    // 2. Config override wins over the heuristic (but not over an explicit
    //    read-only hint below, which the operator can still see).
    if let Some(m) = config_override {
        return m;
    }
    // 1. MCP annotations.
    if let Some(ann) = annotations {
        if let Some(ro) = ann.get("readOnlyHint").and_then(Value::as_bool) {
            return !ro;
        }
        if let Some(dh) = ann.get("destructiveHint").and_then(Value::as_bool) {
            if dh {
                return true;
            }
        }
    }
    // 3. Heuristic on the tool name's words.
    let words: Vec<String> = tool_name
        .split(['_', '-', '.', ' '])
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect();
    let lead = words.first().map(String::as_str).unwrap_or("");

    // Leading read verb (e.g. `get_status`, `list_runs`) is authoritative: these
    // are reads even if a later word looks mutating.
    if READ_VERBS.contains(&lead) {
        return false;
    }
    // Leading write verb, or a strong write verb appearing anywhere (covers
    // object-first names like `pods_delete`, `pull_request_merge`).
    if MUTATION_VERBS.contains(&lead) {
        return true;
    }
    words
        .iter()
        .any(|w| STRONG_MUTATION_VERBS.contains(&w.as_str()))
}

/// Unambiguous write verbs that indicate a mutation no matter where they appear
/// in the tool name (e.g. `pods_delete`, `workflow_run`, `branch_merge`).
const STRONG_MUTATION_VERBS: &[&str] = &[
    "create",
    "delete",
    "remove",
    "update",
    "merge",
    "destroy",
    "terminate",
    "kill",
    "cancel",
    "revert",
    "rollback",
    "apply",
    "scale",
    "restart",
    "deploy",
    "publish",
    "rotate",
    "revoke",
    "grant",
    "drop",
    "truncate",
    "purge",
    "reset",
    "rename",
    "run",
    "exec",
    "execute",
    "trigger",
    "install",
    "uninstall",
    "provision",
    "transition",
    "assign",
    "approve",
    "reject",
];

/// Build a [`PyBinding`] for one tool.
pub fn build_binding(
    server: &str,
    tool_name: &str,
    summary: &str,
    input_schema: &Value,
    output_schema: Option<&Value>,
    is_mutation: bool,
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
    // Universal kwargs (e.g. timeout_ms) are accepted by every SDK function.
    // The generated body forwards them into _args so the dispatcher sees them.
    sig_params.push("**_extra: Any".to_string());

    let ret = return_type(output_schema);
    let signature = format!("def {fn_name}({}) -> {ret}:", sig_params.join(", "));

    PyBinding {
        fn_name,
        server: server.to_string(),
        tool_name: tool_name.to_string(),
        summary: summary.to_string(),
        signature,
        params,
        is_mutation,
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
        let b = build_binding(
            "everything",
            "get-sum",
            "Returns the sum",
            &schema,
            None,
            false,
        );
        assert_eq!(b.fn_name, "everything_get_sum");
        assert!(b.signature.starts_with("def everything_get_sum("));
        assert!(b.signature.contains("a: int"));
        assert!(b.signature.contains("b: int"));
        assert!(b.signature.contains("label: str | None = None"));
        // Every SDK fn accepts universal kwargs (timeout_ms) via **_extra.
        assert!(b.signature.contains("**_extra: Any"));
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

    #[test]
    fn mutation_heuristic_by_verb() {
        // Write verbs.
        assert!(classify_mutation("create_issue", None, None));
        assert!(classify_mutation("delete-pod", None, None));
        assert!(classify_mutation("pods_delete", None, None));
        assert!(classify_mutation("update_pull_request", None, None));
        // Read verbs (including read-verb overrides of ambiguous names).
        assert!(!classify_mutation("get_me", None, None));
        assert!(!classify_mutation("list_applications", None, None));
        assert!(!classify_mutation("search_issues", None, None));
        assert!(!classify_mutation("get_status", None, None));
        // Ambiguous/unknown leading word defaults to read-only.
        assert!(!classify_mutation("foobar", None, None));
        // Object-first names where the verb is a later word.
        assert!(classify_mutation("pods_delete", None, None));
        assert!(classify_mutation("pull_request_merge", None, None));
        assert!(classify_mutation("workflow_run", None, None));
        // Leading read verb wins even if a later word looks mutating.
        assert!(!classify_mutation("get_check_runs", None, None));
        assert!(!classify_mutation("list_deployments", None, None));
        // A read that happens to contain a non-strong word stays read.
        assert!(!classify_mutation("pull_request_read", None, None));
    }

    #[test]
    fn mutation_annotation_wins_over_heuristic() {
        // readOnlyHint:true forces read even for a write-verb name.
        let ann = json!({"readOnlyHint": true});
        assert!(!classify_mutation("delete_thing", Some(&ann), None));
        // readOnlyHint:false forces write even for a read-verb name.
        let ann = json!({"readOnlyHint": false});
        assert!(classify_mutation("get_thing", Some(&ann), None));
        // destructiveHint:true marks write.
        let ann = json!({"destructiveHint": true});
        assert!(classify_mutation("do_thing", Some(&ann), None));
    }

    #[test]
    fn mutation_config_override_wins() {
        // Operator can force a read-verb tool to be treated as a mutation.
        assert!(classify_mutation("get_thing", None, Some(true)));
        // ...or exempt a write-verb tool.
        assert!(!classify_mutation("delete_thing", None, Some(false)));
    }
}

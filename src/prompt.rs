//! Builds the `execute_python` tool description shown to the agent's LLM.
//!
//! Intro + exactly two lines per tool: the typed signature and a one-line
//! summary. This is the only token cost the agent sees.

use std::collections::BTreeMap;

use crate::env::Isolation;
use crate::sdk::SdkRegistry;

/// Build the `execute_python` description.
///
/// `shapes` maps `(server, tool)` to a learned return-shape exemplar. When a
/// binding has a non-empty learned shape, it is appended as a third line under
/// that tool (`# returns: {...}`), so the model stops guessing field names.
/// Pass an empty map to get the classic two-lines-per-tool description.
pub fn build_description(
    registry: &SdkRegistry,
    isolation: Isolation,
    shapes: &BTreeMap<(String, String), String>,
) -> String {
    let mut s = String::new();

    s.push_str(
        "Execute Python code that can call any of the connected MCP tools as ordinary \
         typed functions, then transform and combine their results in a single step.\n\n",
    );
    s.push_str(
        "Independent tool calls run concurrently automatically: each call is dispatched the \
         instant you make it and only blocks when you read its result. To overlap calls, make \
         them before reading any result: ``a = tool1(...); b = tool2(...); use(a, b)`` — both \
         requests are already in flight when you read ``a``. A call you read immediately \
         (``x = tool(...)[\"k\"]``) just blocks like a normal function.\n\n",
    );
    s.push_str(
        "All SDK functions below are ALREADY IMPORTED into your execution namespace — \
         do NOT import them. Call them directly. Return a value by assigning to `result` \
         or leaving it as the last expression; anything printed to stdout is also captured.\n\n",
    );

    match isolation {
        Isolation::HostSystem => s.push_str(
            "Runtime: host CPython (full standard library and installed packages available). \
             No sandbox — runs with the gateway's privileges.\n\n",
        ),
        Isolation::Docker => s.push_str(
            "Runtime: CPython inside an isolated Docker container. Full Python; filesystem \
             and network are container-scoped.\n\n",
        ),
        Isolation::Monty => s.push_str(
            "Runtime: Monty sandbox (a safe Python subset). No third-party libraries, no \
             classes, limited stdlib. Use simple functions and the provided SDK calls.\n\n",
        ),
    }

    s.push_str("Available tools (signature + summary):\n\n");

    for b in &registry.bindings {
        s.push_str(&b.signature);
        s.push('\n');
        s.push_str("    # ");
        s.push_str(&b.summary);
        s.push('\n');
        // Append a learned return shape, if one has been observed for this tool.
        if let Some(shape) = shapes.get(&(b.server.clone(), b.tool_name.clone())) {
            if !shape.is_empty() {
                s.push_str("    # returns: ");
                s.push_str(shape);
                s.push('\n');
            }
        }
    }

    if registry.bindings.is_empty() {
        s.push_str("(no upstream tools connected)\n");
    }

    s
}

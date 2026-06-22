//! Builds the `execute_python` tool description shown to the agent's LLM.
//!
//! Intro + exactly two lines per tool: the typed signature and a one-line
//! summary. This is the only token cost the agent sees.

use crate::env::Isolation;
use crate::sdk::SdkRegistry;

pub fn build_description(registry: &SdkRegistry, isolation: Isolation) -> String {
    let mut s = String::new();

    s.push_str(
        "Execute Python code that can call any of the connected MCP tools as ordinary \
         typed functions, then transform and combine their results in a single step.\n\n",
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
    }

    if registry.bindings.is_empty() {
        s.push_str("(no upstream tools connected)\n");
    }

    s
}

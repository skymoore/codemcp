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
        "Independent tool calls run concurrently automatically: each call is dispatched the \
         instant you make it and only blocks when you read its result. To overlap calls, make \
         them before reading any result: ``a = tool1(...); b = tool2(...); use(a, b)`` — both \
         requests are already in flight when you read ``a``. A call you read immediately \
         (``x = tool(...)[\"k\"]``) just blocks like a normal function.\n\n",
    );
    s.push_str(
        "All SDK functions below are ALREADY IMPORTED into your execution namespace — \
         do NOT import them. Call them directly. Return a value by assigning to `result` \
         or by leaving it as the final expression; anything printed to stdout is also \
         captured. Nested Pending values are auto-resolved before the result is returned, \
         so you never need json.dumps tricks to force resolution.\n\n",
    );
    s.push_str(
        "Single call? Just make it the last line — no `result =` needed:\n\
         ``github_get_file_contents(owner='o', repo='r', path='p')``\n\n",
    );
    s.push_str(
        "Never lose a partial result to one failing call. `gather(...)` resolves many \
         calls without raising and returns the allSettled shape \
         (``[{'ok': True, 'value': ...}, {'ok': False, 'error': {...}}]``):\n\
         ``result = gather(tool_a(...), tool_b(...), tool_c(...))``\n\
         A raised failure is a structured `ToolError` with .kind/.server/.tool/.args. \
         Add a per-call deadline with the `timeout_ms` kwarg on any call.\n\n",
    );
    s.push_str(
        "Writes are gated. Tools that mutate state must be authorized: pass \
         `allow_mutations=[\"<fn_name>\", ...]` alongside your code, or set \
         `dry_run=true` to preview exactly what would change without writing. Every run \
         returns a compact `trace` of the calls it made and a `mutations` audit.\n\n",
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

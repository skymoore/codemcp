# codemcp — Meta-MCP "Code-Mode" Gateway

> A single MCP server that connects to many upstream MCP servers and exposes
> **one tool: `execute_python`**. Agents write Python that calls all upstream
> tools as typed functions and transform results in-process, instead of doing
> many sequential tool calls.

Status: **Plan** (June 2026). Target: Rust + `rmcp` 1.7, Python 3.14 worker.

---

## 1. Concept

```
agent harness ──MCP──► codemcp gateway ──MCP clients──► upstream servers
   (stdio/HTTP)         (one tool: execute_python)       (github, sentry, …)
                              │                            (stdio / streamable-http)
                              ├─ generates typed Python SDK (1 fn per upstream tool)
                              ├─ runs Python in HOST_SYSTEM | DOCKER | MONTY
                              └─ SDK fns call back into gateway → route to upstream client
```

The agent's LLM sees **only** `execute_python`. Its description contains a short
intro plus **two lines per upstream tool**: the full typed Python signature and a
one-line summary. The SDK itself is preloaded into the Python runtime once — it is
**never** concatenated into the per-call code string.

---

## 2. Key Design Decisions (resolved)

| Decision | Choice | Rationale |
|---|---|---|
| SDK injection | **Host functions + preloaded module** | SDK reaches Python as callables / a preloaded module, never as source text per call. Per call only the user's code travels. Solves the `python -c` size limit. |
| Isolation modes (v1) | **HOST_SYSTEM (default), DOCKER, MONTY** | All shipped day one behind one `Executor` trait. HOST has no language limits; DOCKER for untrusted; MONTY opt-in safe sandbox. |
| Upstream call path | **Callback into the Rust gateway** | Single routing path + connection reuse. MONTY uses in-process `external_functions`; HOST/DOCKER use the WebSocket control channel. |
| Tool summaries | **Upstream `description` first, LLM optional** | Default offline. `CODEMCP_ENABLE_LLM_SUMMARIES` condenses via one cached LLM call per tool. |
| Control channel | **Single bidirectional JSON-RPC over WebSocket** | One protocol everywhere (HOST loopback, DOCKER Linux/macOS, future remote workers). WebSocket is natively bidirectional — handles `run` (gateway→worker) and `call_tool` (worker→gateway) on one connection with no SSE/long-poll hacks. Negligible overhead on localhost; clean extension point. |
| Control auth | **First WS message is an auth token** | Shared secret (`CODEMCP_CONTROL_TOKEN`, auto-generated per run if unset). Worker must send the token as its first frame or the gateway closes the connection. Protects the arbitrary-code-exec + upstream-routing channel from other local processes. |
| Control framing | **JSON-RPC 2.0, one object per WS text message** | WebSocket provides message boundaries — no manual line framing. Code rides as a JSON string field. stdout/stderr stay pure for user output. |
| Worker lifecycle | **Persistent worker, fed per call** | SDK imported once at startup; each `execute_python` sends a `run` request. Fast. State reset between calls unless a session is requested. |
| Config location | **`~/.config/codemcp/mcp.json`** (XDG) | Honors `$XDG_CONFIG_HOME`; override with `CODEMCP_CONFIG`. opencode `mcp` object format. |
| Monty maturity | **Opt-in, not default** | v0.0.18 is experimental (no classes, no 3rd-party libs, limited stdlib). Ideal sandbox model but too limited for general transform code. Pin the version; document the Python subset. |

### Control channel: WebSocket (one protocol everywhere)
The gateway runs a WebSocket server (bound loopback by default). The Python worker
connects as a WebSocket client. JSON-RPC 2.0 messages flow both ways on the one
connection:
- gateway → worker: `run { code, session? }`
- worker → gateway: `call_tool { server, tool, args }`

WebSocket is chosen over a Unix socket / TCP+LinesCodec / HTTP surface because:
- **One protocol for every isolation/topology**: HOST loopback, DOCKER on Linux
  *and* macOS (no host-Unix-socket bind-mount issues), and future remote workers.
- **Natively bidirectional** — no SSE/long-poll/dual-server hacks for the
  server→worker direction.
- **Built-in message framing** — no manual length-prefix or line framing.
- **Negligible overhead** on localhost; clean extension point (subprotocols,
  multiple workers, TLS later).

**Auth:** the worker's **first WS message must be the auth token**
(`CODEMCP_CONTROL_TOKEN`; auto-generated per run if unset and injected into the
worker's env). Wrong/missing token → gateway closes the connection. This protects
a channel that both executes arbitrary code and routes to authenticated upstream
MCP servers.

**Rust side:** `tokio-tungstenite` (WS) + `serde_json`. (`jsonrpsee` is *not*
used: its strict client/server split fights our bidirectional-peer model; we keep
JSON-RPC hand-rolled and minimal over the WS messages.)

**Python side:** `bootstrap.py` requires the `websockets` package (pure-Python,
single dependency) but **self-provisions it** — no custom container, no `uv`, no
venv assumption. On startup it does `import websockets`; on `ImportError` it
installs into a private target dir (`pip install --target <dir> websockets`, dir
added to `sys.path`), then imports and runs. This means **any stock `python:*`
image or host python with pip works out of the box**. Provisioning is controllable
via env (offline/pinned scenarios): `CODEMCP_WS_AUTO_INSTALL` (default `true`),
`CODEMCP_WS_PIP_ARGS` (e.g. `--index-url ...`), `CODEMCP_WS_VERSION` (pin).
MONTY mode has no worker and is unaffected.

---

## 3. Project Structure

```
codemcp/
├── Cargo.toml
├── PLAN.md
├── README.md
├── src/
│   ├── main.rs              # CLI, tracing, signals, transport selection, wiring
│   ├── env.rs               # all CODEMCP_* env vars, typed (figment/envy)
│   ├── config.rs            # load mcp.json (opencode format)
│   ├── error.rs             # thiserror Error -> rmcp ErrorData
│   ├── server.rs            # ServerHandler: exposes execute_python only
│   ├── prompt.rs            # builds execute_python description (intro + 2 lines/tool)
│   ├── upstream/
│   │   ├── mod.rs           # UpstreamManager: HashMap<name, peer+tools>, route, shutdown
│   │   └── client.rs        # connect one server (stdio | streamable-http)
│   ├── sdk/
│   │   ├── mod.rs           # SdkRegistry: tools -> Python defs + summaries (+ cache)
│   │   ├── codegen.rs       # JSON Schema -> typed Python stubs + docstrings
│   │   └── summary.rs       # description-first; optional cached LLM condense
│   ├── exec/
│   │   ├── mod.rs           # Executor trait + factory by CODEMCP_ISOLATION
│   │   ├── host.rs          # persistent host python worker
│   │   ├── docker.rs        # persistent container worker
│   │   └── monty.rs         # in-process monty (feature = "monty")
│   └── control.rs           # WebSocket server: bidirectional JSON-RPC + token auth
└── pyworker/
    ├── bootstrap.py         # WS client (websockets pkg): auth, run-loop, SDK loader
    └── (generated) sdk.py   # written at startup from SdkRegistry
```

---

## 4. Dependencies (`Cargo.toml`)

```toml
[package]
name = "codemcp"
version = "0.1.0"
edition = "2021"

[features]
default = []
monty = ["dep:monty"]

[dependencies]
rmcp = { version = "1.7", features = [
  "server", "client", "macros", "schemars",
  "transport-io",                              # server stdio
  "transport-child-process",                   # client stdio (spawn upstream)
  "transport-streamable-http-server",          # server HTTP
  "transport-streamable-http-client-reqwest",  # client HTTP (upstream)
] }
tokio = { version = "1", features = ["full"] }
tokio-tungstenite = "0.24"                        # WebSocket control channel
futures = "0.3"
rand = "0.10"                                      # auto-generate control token
serde = { version = "1", features = ["derive"] }
serde_json = "1"
schemars = "1"
thiserror = "2"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
uuid = { version = "1", features = ["v4"] }       # JSON-RPC request id correlation
which = "8"                                       # locate python3
figment = { version = "0.10", features = ["env", "json"] }  # config + env
reqwest = { version = "0.13", default-features = false, features = ["json", "rustls-tls"] } # LLM summaries
clap = { version = "4", features = ["derive"] }   # optional CLI/subcommands

# optional, feature-gated; pinned (experimental crate)
monty = { version = "=0.0.18", optional = true }
```

Notes:
- `duct` was considered for the worker but `tokio::process::Command` with
  `kill_on_drop(true)` is sufficient and keeps everything async-native; we use it directly.
- `jsonrpsee` intentionally **not** included (see §2 rationale): bidirectional
  peer over WebSocket; JSON-RPC kept hand-rolled and minimal.
- `tokio-tungstenite` rides on the existing tokio stack; no extra runtime.
- Python worker needs the `websockets` package (provisioned via `uv run --with
  websockets` for HOST, baked into the image for DOCKER).

---

## 5. Components

### 5.1 Env / Config (`env.rs`, `config.rs`)
All tunables via env (typed struct, parsed with `figment`):

| Var | Default | Meaning |
|---|---|---|
| `CODEMCP_CONFIG` | `$XDG_CONFIG_HOME/codemcp/mcp.json` (else `~/.config/...`) | Upstream config path |
| `CODEMCP_ISOLATION` | `HOST_SYSTEM` | `HOST_SYSTEM` \| `DOCKER` \| `MONTY` |
| `CODEMCP_TRANSPORT` | `stdio` | Server transport: `stdio` \| `http` |
| `CODEMCP_HTTP_BIND` | `127.0.0.1:8787` | Bind addr for `http` |
| `CODEMCP_PYTHON` | (auto via `which`) | Host python binary |
| `CODEMCP_DOCKER_IMAGE` | `python:3.14-slim` | Image for DOCKER mode |
| `CODEMCP_DOCKER_EXTRA_ARGS` | — | Extra `docker run` args |
| `CODEMCP_CONTROL_BIND` | `127.0.0.1:0` (ephemeral port) | WebSocket control bind addr; worker dials this |
| `CODEMCP_CONTROL_HOST_FOR_WORKER` | (auto) | Host the worker uses to reach the gateway (e.g. `host.docker.internal` for DOCKER on macOS) |
| `CODEMCP_CONTROL_TOKEN` | (auto-generated per run) | Shared secret; worker's first WS frame must equal this |
| `CODEMCP_WS_AUTO_INSTALL` | `true` | Worker self-installs `websockets` if missing |
| `CODEMCP_WS_VERSION` | (latest) | Pin `websockets` version for the worker install |
| `CODEMCP_WS_PIP_ARGS` | — | Extra pip args for the worker install (e.g. `--index-url`) |
| `CODEMCP_EXEC_TIMEOUT_MS` | `30000` | Per-call execution timeout |
| `CODEMCP_MAX_OUTPUT_BYTES` | `1048576` | Cap on captured stdout/stderr |
| `CODEMCP_MONTY_MEM_LIMIT` | `268435456` | Monty memory tracker limit |
| `CODEMCP_ENABLE_LLM_SUMMARIES` | `false` | Use LLM to condense summaries |
| `CODEMCP_SUMMARY_MODEL` / `_API_BASE` / `_API_KEY` | — | LLM summary config |
| `CODEMCP_SUMMARY_CACHE` | `$XDG_CACHE_HOME/codemcp/summaries.json` | Summary cache |
| `CODEMCP_LOG` | `info` | `tracing` env filter |

`config.rs` parses the opencode `mcp` object:
- `type: "local"` → `command: [..]`, `environment: {}`, `cwd?`, `timeout?`, `enabled?`
- `type: "remote"` → `url`, `headers: {}`, `enabled?`, `timeout?`
- Skip entries with `enabled: false`. `{env:VAR}` interpolation in header/env values.

### 5.2 Upstream manager (`upstream/`)
- On startup connect to every enabled server **concurrently**:
  - local → `TokioChildProcess`; remote → `StreamableHttpClientTransport`.
- After init, `list_tools` each; store `Arc<HashMap<server_name, Upstream>>` where
  `Upstream { peer: Peer<RoleClient>, tools: Vec<Tool> }`.
- `route_tool_call(server, tool, args) -> CallToolResult` reuses the live peer.
- Failed upstreams: logged, skipped (never panic). Graceful `shutdown()` on signal.

### 5.3 SDK registry + codegen (`sdk/`)
- One typed Python function per upstream tool: name `"{server}_{tool}"`,
  params from JSON Schema, a return annotation, one-line summary as docstring.
- `codegen.rs`: JSON Schema → Python hints (`str/int/float/bool/list/dict/Optional/Literal`),
  preserve camelCase field names. Emits both the real `sdk.py` (HOST/DOCKER) and
  Monty type-check stubs.
- `summary.rs`: default = first line of upstream `description` (truncated). If
  `CODEMCP_ENABLE_LLM_SUMMARIES`, condense via one LLM call per tool; cache to disk
  keyed by a hash of the tool definition.

### 5.4 The single-tool prompt (`prompt.rs`)
`execute_python`'s description =
1. Short intro: what it's for, that **all SDK functions are already imported** (no
   import needed), results are returned via the last expression / `print`, security
   note for the active isolation mode.
2. **Exactly two lines per tool**:
   - `def github_create_issue(repo: str, title: str, body: str) -> dict:`
   - `    # <one-line summary>`

This is the only token cost shown to the agent's LLM.

### 5.5 Executors (`exec/`) — `Executor` trait
```rust
#[async_trait]
trait Executor: Send + Sync {
    async fn run(&self, code: String) -> Result<ExecOutput, Error>;
    async fn shutdown(&self);
}
struct ExecOutput { result: serde_json::Value, stdout: String, stderr: String }
```
- **host.rs (default):** persistent `python3` running `bootstrap.py`; generated
  `sdk.py` on `PYTHONPATH`. `bootstrap.py` self-provisions `websockets` (see §6),
  then dials the gateway's WS control channel and authenticates with
  `CODEMCP_CONTROL_TOKEN` (passed via env). `run` requests + `call_tool` callbacks
  travel over that WebSocket. `tokio::process::Command` with `kill_on_drop(true)`.
  Enforce `CODEMCP_EXEC_TIMEOUT_MS` and `CODEMCP_MAX_OUTPUT_BYTES`.
- **docker.rs:** same `bootstrap.py` runs inside **any stock** `CODEMCP_DOCKER_IMAGE`
  (default `python:3.14-slim`) — no custom image build. The gateway copies/mounts
  `bootstrap.py` + `sdk.py` in and runs them; `bootstrap.py` self-installs
  `websockets`, connects out to the gateway WS at `CODEMCP_CONTROL_HOST_FOR_WORKER`
  (e.g. `host.docker.internal` on macOS), and authenticates with the token. No
  host-path socket bind-mount needed — the WebSocket crosses the VM/network
  boundary cleanly. OS isolation; ~200ms+install cold start (cacheable).
- **monty.rs (feature = "monty"):** in-process `monty`. SDK = `external_functions`
  map; each closure → `route_tool_call`. Type-check stubs from codegen. Enforce
  mem/time via Monty's tracker. Document the supported Python subset.

### 5.6 Control channel (`control.rs`)
- WebSocket server via `tokio-tungstenite`, bound to `CODEMCP_CONTROL_BIND`
  (loopback, ephemeral port by default). Accepts exactly one worker connection
  per executor (reject/replace extras).
- **Auth handshake:** first text message from the worker MUST equal
  `CODEMCP_CONTROL_TOKEN`. Otherwise close with a policy-violation code. Only after
  a valid token does the JSON-RPC loop start.
- Each WS text message is one JSON-RPC 2.0 object (WS gives message boundaries).
- **Methods:**
  - gateway → worker: `run { code, session? }` → `{ result, stdout, stderr }`
  - worker → gateway: `call_tool { server, tool, args }` → `CallToolResult`
- Request/response correlation via `id` (`uuid` v4). A single task reads frames and
  dispatches — responses matched by id, `call_tool` requests routed to
  `UpstreamManager`.
- MONTY bypasses this entirely (direct in-process calls; no WS, no token).

### 5.7 Server + main (`server.rs`, `main.rs`)
- `ServerHandler` exposing only `execute_python(code: String) -> CallToolResult`.
  Manual handler so the dynamic description from `prompt.rs` is injected at runtime.
- `main.rs`: init tracing → load config → connect upstreams → build SDK registry +
  write `sdk.py` → generate control token + start WS control server → construct
  executor (spawns + authenticates worker) → serve over `stdio` or Streamable HTTP
  per `CODEMCP_TRANSPORT` → `tokio::signal` + `CancellationToken` for graceful
  shutdown (disconnect upstreams, close WS, kill worker).

### 5.8 Error handling (`error.rs`)
- `thiserror` `Error` with `From` impls and `Into<rmcp::model::ErrorData>`.
- Tool handlers always return `Result`; never panic. Upstream/worker failures
  surface as structured tool errors with context.

---

## 6. `pyworker/bootstrap.py` (self-provisioning; stdlib to bootstrap)
- **Self-provision `websockets`** (stdlib-only logic): try `import websockets`; on
  `ImportError` and if `CODEMCP_WS_AUTO_INSTALL` (default true), run
  `python -m pip install --target <cache_dir> websockets[==CODEMCP_WS_VERSION]
  CODEMCP_WS_PIP_ARGS` via `subprocess`, add `<cache_dir>` to `sys.path`, then
  import. `<cache_dir>` is reused across runs so install happens once. If install
  is disabled and the import fails, exit with a clear message.
- Connect to the gateway WS at `CODEMCP_CONTROL_HOST_FOR_WORKER` (host) /
  `host.docker.internal` (docker macOS).
- **Send the auth token as the first frame** (`CODEMCP_CONTROL_TOKEN` from env).
- `import sdk` (generated, on `PYTHONPATH`); SDK functions emit `call_tool`
  JSON-RPC requests over the WS and await the matching response.
- Run loop: receive `run` request → `exec` user code in a fresh namespace (with SDK
  names injected) → capture stdout/stderr → return last-expression value as
  `result` → send response.
- Self-contained: works on HOST and in **any** stock python container with pip.
  Async-native (`asyncio`) so `call_tool` round-trips don't block the loop.

---

## 7. Build Order (phases)

1. [DONE] Scaffold crate + `env.rs` + `config.rs` (parse opencode mcp.json).
2. [DONE] `upstream/` connect + `list_tools` + route (stdio first, then http).
3. [DONE] `sdk/codegen.rs` + registry + `prompt.rs` (2 lines/tool).
4. [DONE] `control.rs` (WS server + token auth) + `pyworker/bootstrap.py`
   (self-provisions `websockets`, WS client) + `exec/host.rs` (spawn plain
   `python3`). Verified end-to-end via `CODEMCP_SMOKE`: Python calls
   `everything_get_sum(a=2,b=40)` → `call_tool` over WS → upstream → result
   transformed in one Python step.
5. `server.rs` + `main.rs`: **stdio server serving `execute_python` → host executor.
   First working vertical slice.**
6. Add Streamable HTTP server transport.
7. `exec/docker.rs`.
8. `exec/monty.rs` (feature-gated) + type-check stubs.
9. Optional LLM summaries + cache.
10. README: env vars, isolation trade-offs, Monty Python-subset limits + security
    boundaries.

---

## 8. Monty Verdict
Not mature enough to be the **default** (no classes, no third-party libs, limited
stdlib → real transform code will frequently break). But its host-function +
sandbox model is the best in-process option and it is safe. Ship **opt-in**
(`CODEMCP_ISOLATION=MONTY`), pinned to `=0.0.18`, with documented limits. Default
stays `HOST_SYSTEM`; **DOCKER** is the recommended isolation for untrusted agents.

---

## 9. Security Boundaries (documented in tool description + README)
- `HOST_SYSTEM`: **no isolation** — full host access. Dev/trusted only.
- `DOCKER`: OS-level isolation; only the authenticated WebSocket control channel
  bridges in. Untrusted code.
- `MONTY`: strict in-process sandbox; no filesystem/network/env except the SDK
  callbacks the gateway grants. Limited Python subset.
- **Control channel auth:** the WS control channel both executes arbitrary code and
  routes to authenticated upstream MCP servers, so it is gated by a per-run shared
  token (`CODEMCP_CONTROL_TOKEN`, first WS frame). Bind loopback by default; never
  expose the control port publicly without TLS + a strong token.

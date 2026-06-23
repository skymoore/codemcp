# codemcp — Meta-MCP "Code-Mode" Gateway

A single MCP server that connects to many upstream MCP servers and exposes
**one tool: `execute_python`**. Agents write Python that calls every upstream
tool as a typed function and transform/combine results in-process — instead of
issuing many sequential MCP tool calls.

```
agent harness ──MCP──► codemcp gateway ──MCP clients──► upstream servers
   (stdio/HTTP)         (one tool: execute_python)       (github, sentry, …)
                              │                            (stdio / streamable-http)
                              ├─ generates a typed Python SDK (1 fn per upstream tool)
                              ├─ runs Python in a worker process
                              └─ SDK fns call back into the gateway → route to upstream
```

The agent's LLM sees **only** `execute_python`. Its description contains a short
intro plus **two lines per upstream tool**: the full typed Python signature and a
one-line summary. The SDK itself is preloaded into the Python runtime once — it is
**never** concatenated into the per-call code string.

## Why

A typical agent task ("find the open issue mentioning X, fetch its linked PR, and
summarize the diff") becomes three or more round-trips through the model, each
re-sending tool schemas and intermediate JSON. With codemcp the agent writes one
Python snippet that calls the three tools and returns just the summary:

```python
issue = github_search_issues(query="X", state="open")[0]
pr = github_get_pull_request(number=issue["linked_pr"])
result = {"title": pr["title"], "files_changed": len(pr["files"])}
```

One model turn, one tool call, only the final result returned.

## Bench: token usage vs direct MCP

A repeatable experiment in [`bench/`](./bench) measures whether the design above
actually saves model tokens. A standalone LangGraph agent answers three
read-only questions over the GitHub user `skymoore`'s data, under two arms that
expose the **identical** GitHub MCP toolset (~41 tools) but differ only in how
it reaches the model:

| arm | what the model sees |
|---|---|
| `direct` | all ~41 GitHub tools bound directly (large per-turn tool schema) |
| `codemcp` | one `execute_python` tool whose description lists ~41 two-line signatures |

Both arms run the same model (`claude-sonnet-4-6` via the OpenCode Zen
Anthropic endpoint, `temperature=0`), the same upstream GitHub MCP server, and a
fresh MCP client + (for codemcp) fresh gateway subprocess per run. Token counts
are the provider's own `usage` figures (Anthropic `usage` block), summed per run.
Each (task, arm) is repeated 3× for variance. Correctness is reviewed manually
against `ground_truth.json`, computed by calling the GitHub tools directly
(no LLM).

### Results (18 runs, all answered correctly)

| task | direct input | codemcp input | Δinput | direct turns | codemcp turns |
|---|---|---|---|---|---|
| A — repo count (1 tool call) | 25,233 | 22,816 | **−2,417 (−10%)** | 2.0 | 2.0 |
| B — most-starred repo's latest commit (2 calls) | 35,779 | 15,981 | **−19,798 (−55%)** | 3.0 | 3.0 |
| C — most-issues repo + README check (2 calls) | 399,456 | 41,702 | **−357,755 (−90%)** | 3.0 | 3.7 |

Headline: on the multi-tool tasks codemcp cut **input** tokens 55–90%; on the
1-tool baseline it's roughly flat. `cache_read` was 0 across all runs (Zen
returned no prompt-cache hits on these short sessions, so this is the
no-caching case — a separate, larger-context run would be needed to measure
caching behavior). Full per-run answers, per-turn usage, and the manual-review
section are in `bench/results/summary.md` after a run.

> One toolset (GitHub) and three tasks — a real data point, not an exhaustive
> benchmark. Re-run it and vary the tasks/toolset yourself.

### Run it yourself

From the repo root:

```sh
cd bench
uv sync                                   # install pinned deps into a local venv
uv run python ground_truth.py             # compute ground_truth.json (no LLM)
uv run python runner.py --smoke           # 6 runs: connectivity check
uv run python runner.py --reset --repeats 3   # full bench: 18 runs
uv run python analyze.py                  # -> results/summary.{md,csv}
cat results/summary.md
```

Prerequisites:

- `codemcp` on your `PATH` (the codemcp arm launches a fresh gateway per run).
- A working Docker daemon (the GitHub MCP server runs in a container).
- An OpenCode Zen API key: sign in at https://opencode.ai/auth and run
  `opencode /connect` once so it's stored in `~/.local/share/opencode/auth.json`.
- A GitHub personal access token in `bench/.env` as `GITHUB_TOKEN` (git-ignored;
  `bench/mcp.github.json` uses `{env:GITHUB_TOKEN}`). One is already there if you
  cloned this setup; otherwise add your own with `repo` + `read:org` scopes.

See [`bench/README.md`](./bench/README.md) for the full methodology, fairness
notes, and file layout.

## Status

Working vertical slice over **stdio** and **Streamable HTTP**:

- Connects to upstream MCP servers (stdio + Streamable HTTP), discovers tools.
- Generates a typed Python SDK from each tool's JSON Schema.
- Exposes a single `execute_python` MCP tool whose description carries the SDK.
- Runs user Python in a persistent host CPython worker; SDK calls round-trip back
  to the gateway over an authenticated WebSocket control channel and are routed to
  the right upstream server.

Runs untrusted agents safely with `CODEMCP_ISOLATION=DOCKER` (the worker runs in
a container; see [Docker isolation](#docker-isolation)). The Monty in-process
sandbox and optional LLM tool summaries are still planned — see
[TODO](#todo--planned-work).

## Install

### One-line install (prebuilt binary)

```sh
curl -fsSL https://raw.githubusercontent.com/basedatum/codemcp/main/install.sh | sh
```

This downloads a prebuilt binary for your OS/arch from
[GitHub Releases](https://github.com/basedatum/codemcp/releases), verifies its
SHA-256 checksum, and installs it to `~/.local/bin` (or `/usr/local/bin`).
Supported platforms: macOS (arm64, x86_64) and Linux (arm64, x86_64).

Useful overrides:

```sh
# pin a version and/or choose the install dir
curl -fsSL https://raw.githubusercontent.com/basedatum/codemcp/main/install.sh \
  | CODEMCP_VERSION=v0.1.0 CODEMCP_BIN_DIR="$HOME/bin" sh
```

| Variable          | Purpose                                            |
| ----------------- | -------------------------------------------------- |
| `CODEMCP_VERSION` | Release tag to install (default: latest)           |
| `CODEMCP_BIN_DIR` | Install directory                                  |
| `CODEMCP_REPO`    | `owner/repo` to download from (default `basedatum/codemcp`) |

> opencode launches `codemcp` by bare name, so the install dir must be on your
> `PATH`. The installer prints the exact line to add if it isn't.

### Build from source

Requires a Rust toolchain.

```sh
make install                 # release build, install onto PATH (/usr/local/bin)
make install PREFIX=~/.local # install somewhere else
make uninstall               # remove it
make help                    # list all targets
```

Or with cargo directly: `cargo install --path .`.

## Quick start

### Set up from an existing harness (opencode)

If you already have MCP servers configured in opencode, let codemcp adopt them:

```bash
codemcp setup opencode
```

This backs up `~/.config/opencode/opencode.json`, **moves its `mcp` section
verbatim** into codemcp's `mcp.json`, and rewrites opencode to launch a single
`codemcp` server instead of all the individual ones. Restart opencode afterward.
(`codemcp` must be on your `PATH`, since opencode launches it by bare name.) Only
`opencode` is supported today; more harnesses can be added later.

### Or configure manually

1. Write a config at `~/.config/codemcp/mcp.json` (XDG; override with
   `CODEMCP_CONFIG`). The format is a subset of opencode's `mcp` object:

   ```json
   {
     "mcp": {
       "everything": {
         "type": "local",
         "command": ["npx", "-y", "@modelcontextprotocol/server-everything"]
       },
       "sentry": {
         "type": "remote",
         "url": "https://mcp.sentry.dev/mcp",
         "headers": { "Authorization": "Bearer {env:SENTRY_TOKEN}" }
       }
     }
   }
   ```

   - `type: "local"` → stdio server launched via `command` (with optional
     `environment`, `cwd`).
   - `type: "remote"` → Streamable HTTP server at `url` (with optional `headers`).
   - Any string value supports `{env:VAR}` interpolation.
   - `"enabled": false` skips an entry.
   - `"timeout": <seconds>` caps how long to wait for that upstream to spawn and
     finish the MCP handshake (default 30s). An upstream that exceeds it is
     logged and skipped rather than blocking startup.

2. Run the gateway:

   ```bash
   # stdio (default) — for an agent harness that launches codemcp as a subprocess
   codemcp

   # Streamable HTTP
   CODEMCP_TRANSPORT=http CODEMCP_HTTP_BIND=127.0.0.1:3388 codemcp
   ```

3. Point your MCP client at it. Inspect the generated SDK and tool description
   without serving:

   ```bash
   CODEMCP_DUMP=1 codemcp
   ```

## Enabling/disabling upstreams at runtime

`mcp.json` is the **boot-time** desired state. While the gateway is running you
can connect or disconnect upstreams **without restarting it** using the admin
subcommands, which talk to the running gateway over its Unix admin socket:

```bash
codemcp list                 # show every configured server + live status
codemcp enable github        # connect 'github' now (runtime only)
codemcp disable github       # disconnect 'github' now (runtime only)
```

```
NAME                   TYPE    DEFAULT   CONNECTED  TOOLS
github                 local   yes       yes        45
brave                  local   no        no         0
```

- `DEFAULT` = the `enabled` flag in `mcp.json` (what connects at boot).
- `CONNECTED` = whether it is connected in the running process right now.

By default admin commands change **only the live process** and do **not** touch
`mcp.json`. To also persist the change as the new boot default, pass
`--make-default`:

```bash
codemcp enable brave --make-default    # connect now AND set enabled:true in mcp.json
codemcp disable github --make-default  # disconnect now AND set enabled:false in mcp.json
```

When an upstream is enabled/disabled, codemcp regenerates the Python SDK,
hot-reloads it into the running worker (no worker restart, no lost state), and
sends a `notifications/tools/list_changed` to connected MCP clients so they
re-read the updated `execute_python` description.

> Note: `--make-default` rewrites `mcp.json` (preserving all values) and may
> reorder keys alphabetically.

Most flags have short forms: `-d` (`--make-default`), `-i` (`--instance`),
`-p` (`--port`), `-H` (`--host`).

## Running multiple gateways (one per harness)

You can point several harnesses at codemcp at once — e.g. opencode **and** LM
Studio. Two ways:

**A. One gateway per harness (default, stdio).** Each harness launches its own
`codemcp` process. These are fully independent: each gets its own upstream
connections, Python worker, and a **per-instance admin socket**
(`~/.config/codemcp/admin-<config-hash>-<pid>.sock`), so they never collide.

Each gateway records which application launched it — from
`CODEMCP_INSTANCE_LABEL` if set (the `setup` command writes `opencode`), else the
auto-detected parent process name (e.g. `lmstudio`). List them and target one:

```bash
codemcp instances            # show every running gateway
# LAUNCHER       PID      CONFIG
# opencode       40012    /Users/you/.config/codemcp/mcp.json
# lmstudio       40988    /Users/you/.config/codemcp/mcp.json

codemcp list -i opencode             # admin commands target a specific gateway
codemcp enable github -i lmstudio    # by launcher name, config substring, or PID
```

When only one gateway is running, `-i` is unnecessary. When several are running,
admin commands require `-i` and otherwise print the list to disambiguate.

**B. One shared gateway over HTTP.** Run a single long-lived gateway on a fixed
port (default `3388`) and point both harnesses at the same URL:

```bash
codemcp start                 # listens on 127.0.0.1:3388
codemcp start --port 3388     # explicit; or -p 3388 -H 0.0.0.0 to expose it
```

`start` runs the Streamable HTTP transport and **fails fast if the port is
already in use**. Configure each harness with a remote MCP entry pointing at
`http://127.0.0.1:3388/mcp`.

Note: a single shared gateway means one Python worker and one SDK behind that
port. Concurrent `execute_python` calls from different harnesses are correlated
correctly (no crosstalk, isolated stdout/result, independent timeouts) and their
tool round-trips overlap, but they share one interpreter (no CPU parallelism)
and one global SDK state (an admin `enable`/`disable` affects all clients).

## How it works

1. **Connect & discover.** On startup codemcp connects to every enabled upstream
   server and lists its tools.
2. **Generate the SDK.** Each tool's JSON Schema becomes a typed Python function
   (`server_tool_name(arg: type, ...)`). Tool names are sanitized to valid Python
   identifiers. The generated `sdk.py` is validated as parseable Python.
3. **Expose one tool.** The gateway serves a single `execute_python` tool. Its
   description is the intro + two lines per upstream tool (signature + summary).
4. **Execute.** A persistent Python worker process imports `sdk.py` once. Each
   `execute_python` call sends the user's code to the worker, which runs it and
   returns `{ result, stdout, stderr }`. Assign to `result` (or leave a final
   expression) to return a value.
5. **Route SDK calls.** When user code calls an SDK function, the worker sends a
   `call_tool` request back to the gateway over the WebSocket control channel; the
   gateway forwards it to the right upstream MCP server and returns the result.

### Control channel

The gateway runs a WebSocket server (loopback by default). The worker connects as
a client and, as its **first message**, sends a shared auth token
(`CODEMCP_CONTROL_TOKEN`, auto-generated per run if unset). JSON-RPC 2.0 messages
then flow both ways on the one connection:

- gateway → worker: `run { code }`
- worker → gateway: `call_tool { server, tool, args }`

One protocol covers host loopback, future Docker workers (Linux + macOS), and
future remote workers, and is natively bidirectional with built-in message
framing.

### Self-provisioning worker

`bootstrap.py` provisions its own `websockets` dependency (into a cache dir via
`pip install --target`) if it is missing, so the worker runs on any stock Python
host or container without a custom image. Controlled by `CODEMCP_WS_*`.

## Configuration

All settings are read once at startup from `CODEMCP_*` environment variables.

### Core

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_CONFIG` | `~/.config/codemcp/mcp.json` | Path to the upstream `mcp.json`. |
| `CODEMCP_ISOLATION` | `HOST_SYSTEM` | Execution isolation: `HOST_SYSTEM` (default) or `DOCKER` (containerized). `MONTY` is not implemented yet. |
| `CODEMCP_TRANSPORT` | `stdio` | Downstream MCP transport: `stdio` or `http`. |
| `CODEMCP_ADMIN_SOCKET` | _(per-instance)_ | Override the admin socket path. By default each gateway uses `~/.config/codemcp/admin-<config-hash>-<pid>.sock` so multiple instances don't collide; set this to pin an explicit path. |
| `CODEMCP_INSTANCE_LABEL` | _(auto)_ | Friendly name for this gateway in `codemcp instances`/`list` (e.g. `opencode`). Falls back to the auto-detected parent process name. |
| `CODEMCP_LOG` | `info` | Tracing filter (e.g. `info`, `debug`, `codemcp=debug`). |
| `CODEMCP_PYTHON` | _(auto)_ | Path to the Python interpreter (defaults to `python3`/`python` on `PATH`). |

### Streamable HTTP transport

The HTTP server binds a **fixed, reliable port** (it does **not** fall back to a
random port). If the port is already taken — e.g. another codemcp instance — the
gateway fails to start with a clear error instead of silently moving. Use
`codemcp start -p <port>` for the common case, or set `CODEMCP_TRANSPORT=http`
plus the variables below.

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_HTTP_BIND` | `127.0.0.1:3388` | Address to bind the HTTP server. (`codemcp start --port/--host` overrides this.) |
| `CODEMCP_HTTP_PATH` | `/mcp` | URL path the MCP endpoint is mounted at. |
| `CODEMCP_HTTP_JSON_RESPONSE` | `false` | `true` = stateless plain `application/json` replies; `false` = stateful SSE with session IDs. |

### Control channel

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_CONTROL_BIND` | `127.0.0.1:0` | Address for the WebSocket control server (`:0` = ephemeral port). |
| `CODEMCP_CONTROL_HOST_FOR_WORKER` | _(auto)_ | Host the worker uses to reach the control server. Auto-derived per isolation mode (loopback for HOST; bridge gateway or `host.docker.internal` for DOCKER); set to override for unusual topologies. |
| `CODEMCP_CONTROL_TOKEN` | _(random per run)_ | Shared secret the worker must send as its first WS frame. |

### Worker dependency provisioning

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_WS_AUTO_INSTALL` | `true` | Self-install `websockets` into a cache dir if missing. |
| `CODEMCP_WS_VERSION` | _(unset)_ | Pin the `websockets` version. |
| `CODEMCP_WS_PIP_ARGS` | _(empty)_ | Extra args passed to `pip install` (whitespace-split). |

### Execution limits

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_EXEC_TIMEOUT_MS` | `30000` | Per-`run` execution timeout in milliseconds. |
| `CODEMCP_MAX_OUTPUT_BYTES` | `1048576` | Max captured stdout/stderr bytes. |

### Docker isolation

Set `CODEMCP_ISOLATION=DOCKER` to run the Python worker inside an isolated
container instead of the host process. The worker uses the **same** `bootstrap.py`
and the same WebSocket control protocol; only the spawn mechanism and the
control-channel networking differ. Requires a running Docker (or Podman) daemon
and a binary built with the `docker` feature (on by default).

Cold start installs `websockets` fresh in the container and (on first use) pulls
the image, so the first `execute_python` call is slower than on the host.

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_DOCKER_IMAGE` | `python:3.14-slim` | Image the worker runs in. Any stock python image with `pip` works; pulled automatically if missing. |
| `CODEMCP_DOCKER_NETWORK` | `codemcp-net` | User-defined bridge network the worker attaches to. The control channel binds to this network's gateway only (see security notes). |
| `CODEMCP_DOCKER_MEMORY` | `0` | Hard memory limit in bytes (`0` = unlimited). |
| `CODEMCP_DOCKER_CPUS` | `0` | CPU limit in cores, e.g. `1.5` (`0` = unlimited). |
| `CODEMCP_DOCKER_PIDS_LIMIT` | `0` | Max processes in the container (`0` = unlimited). |
| `CODEMCP_DOCKER_READONLY` | `false` | Mount the container root filesystem read-only. |

The container is always created with hardening defaults: `--rm` (auto-remove),
all Linux capabilities dropped, `no-new-privileges`, attached only to the
dedicated bridge network, and the worker files bind-mounted **read-only**.

> The previous `CODEMCP_DOCKER_EXTRA_ARGS` knob is gone: codemcp talks to the
> Docker API directly (no `docker run` CLI), so limits are set with the typed
> variables above.

**On macOS / Windows (Docker Desktop):** the worker workdir is materialized under
`~/.cache/codemcp/work/<pid>` (inside `$HOME`, which Docker Desktop shares by
default) rather than `$TMPDIR`. If you point `CODEMCP_DOCKER_IMAGE` at a custom
setup that needs other host paths, add them to Docker Desktop's file sharing.

### Monty isolation (planned — see TODO)

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_MONTY_MEM_LIMIT` | `268435456` | Memory limit (bytes) for the Monty sandbox. |

### LLM tool summaries (planned — see TODO)

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_ENABLE_LLM_SUMMARIES` | `false` | Condense upstream tool descriptions via one cached LLM call per tool. |
| `CODEMCP_SUMMARY_MODEL` | _(unset)_ | Model to use for summaries. |
| `CODEMCP_SUMMARY_API_BASE` | _(unset)_ | API base URL for the summary model. |
| `CODEMCP_SUMMARY_API_KEY` | _(unset)_ | API key for the summary model. |
| `CODEMCP_SUMMARY_CACHE` | `~/.cache/codemcp/summaries.json` | Summary cache file. |

## Isolation modes & security boundaries

The `execute_python` tool runs arbitrary code **and** can call any connected
upstream MCP server. Choose isolation based on how much you trust the agent.

| Mode | Status | Isolation | Use when |
|---|---|---|---|
| `HOST_SYSTEM` | **implemented** | **None** — full host access with the gateway's privileges, full stdlib + installed packages. | Development / trusted agents only. |
| `DOCKER` | **implemented** | OS-level container; only the authenticated WebSocket control channel bridges in, and that channel is never exposed to the LAN. | Untrusted agents (recommended). |
| `MONTY` | planned | Strict in-process sandbox: no filesystem/network/env except the SDK callbacks the gateway grants. Limited Python subset (no classes, no third-party libs, partial stdlib). | Maximum safety; simple transform code. |

- **`HOST_SYSTEM` has no sandbox.** It executes with the gateway's privileges.
  Run it only with agents and tools you trust.
- **Control channel auth.** Because the control channel both executes arbitrary
  code and routes to authenticated upstreams, it is gated by a per-run shared
  token (`CODEMCP_CONTROL_TOKEN`, sent as the first WS frame). It binds loopback by
  default. **Never** expose the control port publicly without TLS and a strong
  token.
- **DOCKER channel is not LAN-exposed.** The control channel is effectively
  god-mode over every connected upstream, so codemcp never binds it to `0.0.0.0`.
  On native Linux it binds the dedicated bridge network's **gateway IP** (a
  host-internal interface that is not routed to your physical network). On Docker
  Desktop (macOS/Windows), where the host can't bind the bridge IP, it binds
  **loopback** and lets the container reach it via `host.docker.internal` (which
  Docker Desktop forwards to host loopback). Either way, only the worker
  container — not other machines on the same Wi‑Fi — can reach the port.
- **HTTP transport.** The Streamable HTTP server validates the `Host` header
  against a loopback allow-list by default to prevent DNS-rebinding attacks. Set
  appropriate hosts/origins before any non-loopback deployment, and front it with
  TLS + authentication.

## TODO / planned work

These phases are designed but not yet implemented. Configuration knobs already
exist (see tables above) but are inert until the backends land.

### Phase 8 — Monty isolation (`exec/monty.rs`, feature-gated)

An in-process, safe-by-construction sandbox using
[Monty](https://github.com/pydantic/monty) (pinned to `=0.0.18`). SDK calls are
exposed as Monty `external_functions` rather than over the WebSocket. Opt-in via
`CODEMCP_ISOLATION=MONTY` and the `monty` cargo feature (off by default; the crate
is pulled from git). Monty is a limited Python subset (no classes, no third-party
libraries, partial stdlib), so it suits simple transform code, not arbitrary
scripts. Memory bounded by `CODEMCP_MONTY_MEM_LIMIT`.

### Phase 9 — LLM tool summaries + cache

Optionally condense verbose upstream tool descriptions into a single tight summary
line per tool via one cached LLM call (`CODEMCP_ENABLE_LLM_SUMMARIES`,
`CODEMCP_SUMMARY_*`). Default behavior stays fully offline, using each tool's own
`description`.

## Development

```bash
cargo build
cargo test

# Inspect generated SDK + tool description for a given config
CODEMCP_CONFIG=/path/to/mcp.json CODEMCP_DUMP=1 cargo run

# One-shot smoke test: run a Python snippet against the worker and exit
CODEMCP_CONFIG=/path/to/mcp.json \
  CODEMCP_SMOKE='print(everything_get_sum(a=2, b=40))' cargo run
```

Requires a Python 3 interpreter on `PATH` (3.14 tested) and, for stdio upstreams,
whatever launcher their `command` needs (e.g. `npx`, `uvx`).

## License

MIT

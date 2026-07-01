# codemcp — Meta-MCP "Code-Mode" Gateway

A single MCP server that connects to many upstream MCP servers and exposes
**one tool: `execute_python`**. Agents write Python that calls every upstream
tool as a typed function and transform/combine results in-process — instead of
issuing many sequential MCP tool calls. SDK calls are dispatched concurrently:
every call fires its request the moment it is made and blocks only when its
value is actually read, so independent calls overlap automatically.

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

Independent calls overlap automatically — both requests are on the wire before
either result is read, no special syntax needed:

```python
issues  = github_search_issues(query="bug", state="open")
commits = github_list_commits(repo="codemcp", sha="main")
result  = {"issue_count": issues["total_count"], "latest": commits[0]["sha"]}
```

Live measurement: 4 independent GitHub API calls overlap to **~1.48 s** vs
**~4.26 s** when each result is read before the next call is made — a **~2.9×
speedup** on real network round-trips, with the agent writing ordinary Python.

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
- Authenticates to OAuth-protected remote MCP servers via a full OAuth 2.1
  browser flow (`codemcp auth <name>`). Tokens auto-refresh and persist across
  restarts.

Runs untrusted agents safely with `CODEMCP_ISOLATION=DOCKER` (the worker runs in
a container; see [Docker isolation](#docker-isolation)). The Monty in-process
sandbox and optional LLM tool summaries are still planned — see
[TODO](#todo--planned-work).

## Install

### One-line install (prebuilt binary)

```sh
curl -fsSL https://raw.githubusercontent.com/skymoore/codemcp/main/install.sh | sh
```

This downloads a prebuilt binary for your OS/arch from
[GitHub Releases](https://github.com/skymoore/codemcp/releases), verifies its
SHA-256 checksum, and installs it to `~/.local/bin` (or `/usr/local/bin`).
Supported platforms: macOS (arm64, x86_64) and Linux (arm64, x86_64).

Useful overrides:

```sh
# pin a version and/or choose the install dir
curl -fsSL https://raw.githubusercontent.com/skymoore/codemcp/main/install.sh \
  | CODEMCP_VERSION=v0.1.0 CODEMCP_BIN_DIR="$HOME/bin" sh
```

| Variable          | Purpose                                            |
| ----------------- | -------------------------------------------------- |
| `CODEMCP_VERSION` | Release tag to install (default: latest)           |
| `CODEMCP_BIN_DIR` | Install directory                                  |
| `CODEMCP_REPO`    | `owner/repo` to download from (default `skymoore/codemcp`) |

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

**Feature flags:**

| Feature | Default | What it adds | How to enable / disable |
|---|---|---|---|
| `docker` | **on** | Docker isolation backend (bollard) | Disable: `--no-default-features` |
| `tui` | off | Interactive terminal UI (`codemcp tui`) | Enable: `--features tui` |

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
       },
       "github": {
         "type": "remote",
         "url": "https://api.githubcopilot.com/mcp/",
         "tools": {
           "noisy_tool":   { "enabled": false },
           "another_tool": { "enabled": false }
         }
       },
       "linear": {
         "type": "remote",
         "url": "https://mcp.linear.app/mcp",
         "oauth": { "scope": "read" }
       }
     }
   }
   ```

   - `type: "local"` → stdio server launched via `command` (with optional
     `environment`, `cwd`).
   - `type: "remote"` → Streamable HTTP server at `url` (with optional `headers`).
   - Any string value supports `{env:VAR}` interpolation.
   - `"enabled": false` skips a server at boot.
   - `"tools": { "<name>": { "enabled": false } }` hides individual tools by
     default. Omitted tools default to enabled. See
     [Enabling/disabling tools](#enablingdisabling-tools-at-runtime) below.
   - `"tools": { "<name>": { "mutation": true|false } }` overrides how a tool is
     classified for write-safety. `true` forces it to require `allow_mutations`;
     `false` exempts it. Omitted → auto-classified from MCP annotations + a
     name-verb heuristic. See [Write safety](#write-safety-mutation-gating-dry-run-and-audit-trail).
   - `"timeout": <seconds>` caps how long to wait for that upstream to spawn and
     finish the MCP handshake (default 30s). An upstream that exceeds it is
     logged and skipped rather than blocking startup.
   - `"oauth"` controls OAuth 2.1 authentication for remote servers — see
     [Authenticating remote MCP servers](#authenticating-remote-mcp-servers-oauth).

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
NAME                   TYPE    DEFAULT   CONNECTED  AUTH              TOOLS
github                 remote  yes       yes        authenticated     45
linear                 remote  yes       no         needs_auth (Run: codemcp auth linear)  0
brave                  local   no        no         n/a               0
```

- `DEFAULT` = the `enabled` flag in `mcp.json` (what connects at boot).
- `CONNECTED` = whether it is connected in the running process right now.
- `AUTH` = OAuth authentication status for remote servers (see below).
- `TOOLS` = count of **effective** tools (connected and not individually disabled).

By default admin commands change **only the live process** and do **not** touch
`mcp.json`. To also persist the change as the new boot default, pass
`--make-default` (short: `-d`):

```bash
codemcp enable brave --make-default    # connect now AND set enabled:true in mcp.json
codemcp disable github --make-default  # disconnect now AND set enabled:false in mcp.json
```

When an upstream is enabled/disabled, codemcp regenerates the Python SDK,
hot-reloads it into the running worker (no worker restart, no lost state), and
sends a `notifications/tools/list_changed` to connected MCP clients. Clients that
honor the notification re-read the updated `execute_python` description on their
next `tools/list`; snapshot-once clients (which list tools only at startup) keep
the old description until the next session.

> Note: `--make-default` rewrites `mcp.json` (preserving all values) and may
> reorder keys alphabetically.

## Enabling/disabling tools at runtime

You can enable or disable **individual tools** within a connected server without
touching the server itself. Disabling a tool removes it from the SDK and the
`execute_python` description — the LLM can no longer call it — while the server
stays connected so its other tools keep working.

```bash
codemcp tools                                    # list all tools across all servers
codemcp enable-tool github create_repository     # expose one tool (session only)
codemcp disable-tool github delete_repository    # hide one tool (session only)
codemcp disable-tool github delete_repository -d # hide AND persist to mcp.json
```

```
SERVER             TOOL                       ON      DEFAULT  SESSION  SUMMARY
github             create_issue               yes     on       -        Create a GitHub issue
github             delete_repository          off     off      -        Delete a repository
github             list_commits               yes     on       s        List commits on a branch
```

- `ON` = the **effective** state (what the model currently sees).
- `DEFAULT` = the persisted default from `mcp.json` (`on` if unset).
- `SESSION` = an in-memory override for this gateway run only (`s` = overridden, `-` = follows default).

**Session vs. default:**

| Flag | Effect |
|---|---|
| _(no flag)_ | Change the live process only; reverts to config default on next restart. |
| `--make-default` / `-d` | Also writes `tools.<name>.enabled: true/false` to `mcp.json`; persists across restarts. |

Enabling a tool on a disconnected server auto-connects it first. Disabling a tool
never disconnects its server.

## Authenticating remote MCP servers (OAuth)

Remote MCP servers that require OAuth 2.1 authentication are supported
out of the box. codemcp auto-detects when a server requires auth — no
config change needed unless the server needs a pre-registered client ID.

### Auto-detection

For any remote server without an `Authorization` header and without
`oauth: false`, codemcp automatically:

1. Attempts a plain connection.
2. If the server signals that auth is required, discovers its OAuth
   authorization server metadata (RFC 8414 / RFC 9728).
3. Reports the server as `needs_auth` in `codemcp list` with a hint.

```
NAME     TYPE    DEFAULT  CONNECTED  AUTH                                    TOOLS
linear   remote  yes      no         needs_auth (Run: codemcp auth linear)   0
```

### Authenticating

```bash
codemcp auth linear          # start OAuth flow → opens browser, waits for callback
codemcp auth                 # list all remote servers and their auth status
codemcp auth --list          # same
codemcp logout linear        # remove stored credentials and disconnect
```

`codemcp auth <name>` runs the full OAuth 2.1 authorization code flow with
PKCE (RFC 7636):
1. Discovers the server's authorization server metadata.
2. Dynamically registers a client if the server supports it (RFC 7591), or
   uses the configured `clientId` if provided.
3. Starts a temporary localhost callback server on an ephemeral port.
4. Prints the authorization URL and opens it in your browser.
5. Waits up to 5 minutes for the browser to complete the flow and redirect
   to the callback URL.
6. Exchanges the authorization code for access + refresh tokens.
7. Persists the tokens to `~/.config/codemcp/mcp-auth.json` (mode 0600).
8. Reconnects the upstream using the new credentials.

Tokens are **auto-refreshed** on each request (the gateway handles this
transparently) and **re-persisted** after each refresh, so subsequent
gateway restarts pick up the latest token without re-authenticating.

### OAuth config

By default codemcp uses auto-detection and dynamic client registration. Add
an `oauth` block to a remote server entry to customize:

```json
{
  "mcp": {
    "linear": {
      "type": "remote",
      "url": "https://mcp.linear.app/mcp",
      "oauth": {
        "scope": "read write",
        "callbackPort": 19876
      }
    },
    "custom-server": {
      "type": "remote",
      "url": "https://mcp.example.com/mcp",
      "oauth": {
        "clientId": "my-pre-registered-client-id",
        "clientSecret": "{env:MCP_CLIENT_SECRET}",
        "scope": "mcp:read",
        "redirectUri": "http://127.0.0.1:19876/callback"
      }
    },
    "bearer-only": {
      "type": "remote",
      "url": "https://mcp.example.com/mcp",
      "oauth": false,
      "headers": { "Authorization": "Bearer {env:API_TOKEN}" }
    }
  }
}
```

| Field | Default | Description |
|---|---|---|
| `oauth` (absent) | — | Auto-detect: try OAuth if the server requires it. |
| `oauth: true` | — | Same as absent — explicit auto-detect. |
| `oauth: false` | — | Disable OAuth; use `headers`/bearer only. |
| `oauth.clientId` | — | Pre-registered OAuth client ID. If absent, dynamic registration (RFC 7591) is attempted. |
| `oauth.clientSecret` | — | OAuth client secret (if required by the authorization server). Supports `{env:VAR}`. |
| `oauth.scope` | _(auto)_ | OAuth scopes to request. If absent, the SDK selects scopes from the server's metadata. |
| `oauth.callbackPort` | _(ephemeral)_ | Port for the local OAuth callback server. Use a fixed port when a pre-registered `clientId` has a specific redirect URI. |
| `oauth.redirectUri` | _(derived from port)_ | Full OAuth redirect URI. Overrides `callbackPort`. Must match what's registered with the authorization server. |

**Bearer tokens (`Authorization` header) take priority** — if the server
entry has an `Authorization` header, OAuth is skipped regardless of the
`oauth` field.

### Token storage

OAuth credentials are stored in `~/.config/codemcp/mcp-auth.json`
(mode 0600, file-locked against concurrent writes). The file is keyed by
server name and includes the access token, refresh token, granted scopes,
and the server URL (to detect config changes). Removing credentials:

```bash
codemcp logout linear          # revoke + delete via the running gateway
# or directly:
# delete the entry from ~/.config/codemcp/mcp-auth.json
```

## Interactive TUI

`codemcp tui` opens a full-screen terminal interface for managing servers and tools
against a running gateway — useful when you want to explore what's exposed and
flip things interactively without typing subcommands.

```bash
codemcp tui             # auto-selects the single running gateway
codemcp tui -i opencode # target a specific gateway by launcher name, config, or PID
```

**Layout:** left pane lists servers; right pane lists tools of the selected server.

```
┌ Servers ──────────────────────┐┌ Tools — github ────────────────────────────────────────────┐
│▸ ● github   local  def:on 45t ││▸ ON    create_issue               Create a GitHub issue    │
│  ● sentry   remote def:on 12t ││  ON    list_pull_requests         List pull requests        │
│  ○ brave    local  def:off 0t ││  off   delete_repository          Delete a repository       │
└───────────────────────────────┘└────────────────────────────────────────────────────────────┘
 [opencode] pid 40012 ~/.config/codemcp/mcp.json  github/delete_repository disabled [session]
 Space:session  D:default  Tab:pane  j/k:move  r:refresh  ?:help  q:quit
```

**Keybindings:**

| Key | Action |
|---|---|
| `j` / `↓` | Move selection down |
| `k` / `↑` | Move selection up |
| `Tab` | Switch focus between servers and tools panes |
| `Space` / `Enter` | Toggle the selected item (session only — reverts on restart) |
| `D` | Toggle the selected item and persist as the new default in `mcp.json` |
| `r` | Refresh now (the UI also auto-refreshes every 3 s) |
| `?` | Show/hide help overlay |
| `q` / `Ctrl-C` | Quit |

`●` = server connected; `○` = disconnected. Tools show `(s)` when a session
override is active, `(d)` when the configured default is off.

The TUI requires the `tui` cargo feature (on by default). Build without it to get
a smaller gateway-only binary: `cargo build --release --no-default-features --features docker`.

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
5. **Pre-flight validation.** Before running, the worker statically checks the
   code against the live SDK contract (see below). If it doesn't pass, the call
   returns a structured hint *without executing*, so the model fixes it in the
   same turn instead of paying for a wasted execution round-trip.
6. **Route SDK calls — concurrently.** Each SDK call sends its `call_tool`
   request over the control channel immediately and returns a `Pending` handle
   that resolves on first use. Calls made before any result is read are already
   in flight by the time the first value is accessed. The gateway dispatches each
   `call_tool` on its own async task so upstream round-trips overlap rather than
   serializing.

### Pre-flight validation

Code-mode's win — *one model turn, one tool call* — only holds when the agent
writes correct Python the first time. A typo, a wrong keyword argument, or a
missing required argument otherwise costs a full execution round-trip (run
broken code → return a raw traceback → retry on a more expensive turn),
quietly reintroducing the multi-turn loops the gateway exists to remove.

So before executing, the worker statically checks the code against the **live
SDK contract** (the real generated functions, via `inspect.signature`, so the
check can never drift from what the model sees). It catches:

- **Syntax errors** — reported with a line/column and the offending text.
- **Misspelled SDK calls** — `github_serch_issues(...)` →
  *"is not a known SDK function. Did you mean `github_search_issues`?"*
- **Unknown keyword arguments** — `github_search_issues(stat="open")` →
  *"unknown argument `stat`. Did you mean `state`?"* (or lists the valid args).
- **Missing required arguments** — names exactly which ones are absent.
- **Unauthorized mutations** (only when `CODEMCP_ENFORCE_MUTATIONS=true`) — a
  call to a write tool that is not listed in `allow_mutations` → *"is a mutating
  (write) tool and was not authorized. Re-send with this call listed in
  `allow_mutations`, or use `dry_run: true`."*

If anything fails, the call returns the collected hints in the `error` field
**without running any code**. The check is deliberately conservative: locally
defined functions, builtins, attribute calls (`x.foo()`), `**kwargs` spreads,
comprehensions, and imports are never flagged — only high-confidence mistakes
against the SDK surface are. There is **zero** steady-state cost: the model's
tool description is unchanged, and validation is invisible unless the code is
actually broken.

### Return shapes (`CODEMCP_LEARN_SHAPES`, on by default)

The flip side of pre-flight validation: even with valid calls, the model often
has to *guess the structure of a return value* (`issue["user"]["login"]`) and
pays a retry when it guesses wrong. Tool signatures advertise argument types but
usually return `-> dict[str, Any]`, and declared `outputSchema`s are mostly
absent (and far too verbose to show every turn).

Shape-learning is **on by default** (`CODEMCP_LEARN_SHAPES=true`): each
successful tool call teaches the gateway the structure of what it actually
returns. That knowledge is used through **two independent tiers**, together
gated by the one flag. Set `CODEMCP_LEARN_SHAPES=false` to turn both off, making
the `call_tool` path and worker behavior byte-identical to a no-shape build.

**Tier 1 — strict pre-flight field validation (works with every client).**
This is the high-leverage path. The gateway retains the *full* nested key
structure of each tool's return value — uncapped, unioned across observed entity
variants (e.g. GitHub User vs Organization), and seeded from any declared
`outputSchema` so even the first call is covered — and ships it to the worker.
The pre-flight validator then rejects a wrong literal field access *before
executing*, the same way it already rejects a bad argument:

```
result = github_get_me()
login = result["lgoin"]      # ← rejected, not executed:
# github_get_me: result has no field `lgoin` (line 2). Did you mean `login`?
```

Validation descends through **literal array indexing** too, so the common
list/search-result pattern is covered to any depth:

```
repos = github_search_repositories(query="...")
stars = repos["items"][0]["stargazers"]   # ← rejected, not executed:
# github_search_repositories: result['items'][0] has no field `stargazers`
#   (line 2). Did you mean `stargazers_count`?
```

This turns a wasted round-trip (run → `KeyError` traceback → retry on a more
expensive turn) into a precise same-turn correction. Because it rides the
**worker**, not the tool list, it reaches the model on every turn regardless of
client behavior, has **no prompt-cache impact**, and the full key structure
**never enters the model's context**. It is conservative by construction: only
simple `x = tool(...)` bindings are tracked, only literal `x["k"]` / `x.k` access
and literal integer indices (`x[0]`) are checked, and anything dynamic (`x[i]`),
reassigned, or not-yet-learned is left alone.

**Tier 2 — lossy shape in the description (discovery; client-dependent).**
The first call also learns a small, size-bounded exemplar appended to that tool's
entry in the `execute_python` description, so the model can *discover* field
names up front:

```
def github_get_me() -> dict[str, Any]:
    # Get details of the authenticated user.
    # returns: {avatar_url: str, details: {bio: str, followers: int, ...}, id: int, login: str}
```

This exemplar is **bounded by construction** (depth-capped, fields-capped, arrays
collapsed to one element, hard length cap) — a projection, not a schema, so it
can't reimport the bloat it exists to avoid.

> **Scope of Tier 2 (measured — see [`bench/`](./bench)):** a learned *description*
> shape only reaches the model on a *subsequent* `tools/list`, so its usefulness
> depends on the **client's** tool-listing lifecycle — not on MCP, the transport,
> or the gateway:
> - **Snapshot-once clients** (most, e.g. `langchain-mcp-adapters`) list tools
>   once and ignore `tools/list_changed`, so the session never sees the shape it
>   learned — a no-op, but with **no prompt-cache downside** (the cached prefix is
>   never mutated).
> - **Spec-compliant clients** that re-list *do* see it mid-session (bench arm
>   `codemcp_shapes_relist`, verified) — but that bought **no reliable turn
>   saving** and **did bust the prompt cache** (~+4.3k `cache_creation`, ~−4.3k
>   `cache_read` per task as the tool-schema prefix is re-created).
> - It does reach **later sessions of a shared `codemcp start` HTTP gateway** —
>   a real but cross-session-only benefit.
>
> Tier 1 (validation) has **none** of these caveats: it works with every client
> on every turn and never touches the cache. That's why the validation tier is
> the reason to enable shape-learning; the description tier is a bonus where the
> client lifecycle allows it.

Both tiers are **lazy**: nothing is learned, shipped, or shown until a tool is
actually called, and the steady-state tool list is unchanged between calls. To
disable entirely, set `CODEMCP_LEARN_SHAPES=false`.

> **Get the most out of it: run codemcp as a standalone HTTP gateway.** Both
> tiers compound across calls, and Tier 2's only client-independent payoff is
> *cross-session*. Running one long-lived gateway —
> `codemcp start` (Streamable HTTP, default `127.0.0.1:3388/mcp`) — and pointing
> your clients at it means shapes learned in one session (and by one client) are
> already known to the next. A fresh stdio subprocess per client starts cold
> every time and learns nothing across sessions. Point an MCP client at the
> running gateway with a `url` instead of a `command`:
>
> ```jsonc
> { "mcpServers": { "codemcp": { "url": "http://127.0.0.1:3388/mcp" } } }
> ```

### Control channel

The gateway runs a WebSocket server (loopback by default). The worker connects as
a client and, as its **first message**, sends a shared auth token
(`CODEMCP_CONTROL_TOKEN`, auto-generated per run if unset). JSON-RPC 2.0 messages
then flow both ways on the one connection:

- gateway → worker: `run { code }`
- worker → gateway: `call_tool { server, tool, args }`

The worker sends `call_tool` requests without waiting for responses, so a burst
of calls from agent code travels over the socket concurrently. On the gateway
side each incoming `call_tool` is dispatched on its own async task, so upstream
round-trips overlap; responses flow back in any order, matched to their pending
requests by JSON-RPC id.

One protocol covers host loopback, future Docker workers (Linux + macOS), and
future remote workers, and is natively bidirectional with built-in message
framing.

### Concurrent tool calls

Every SDK call dispatches its request immediately and returns a `Pending` handle
instead of blocking. The handle resolves transparently on first access (subscript,
attribute, iteration, `str()`, equality), so from the agent's perspective an SDK
call looks and behaves like a normal function call — it just happens to overlap
with any other calls that were made before its value was read.

Concurrency therefore needs no special construct: a list comprehension that issues
many calls and then reads them runs those calls in parallel.

```python
# All requests go out first; reading them resolves whatever is already in flight.
pages = [github_get_file_contents(repo=r, path="README.md") for r in repo_list]
texts = [p["content"] for p in pages]
```

`Pending` and `ToolError` are injected into the execution namespace. `ToolError`
is raised when a call fails at the protocol level (transport error or JSON-RPC
error from the gateway). API-level failures from upstream servers (e.g. HTTP 404)
come back as text content inside a successful MCP response — those surface as
strings, not `ToolError`. `Pending.result()` forces explicit resolution, which is
useful when you need to catch errors before the value is read:

```python
p = github_get_file_contents(owner="org", repo="r", path="f")
try:
    val = p.result()
except ToolError as e:
    val = None
```

`ToolError` is **structured**: `e.kind` (`timeout`/`auth`/`transport`/
`not_found`/`upstream_error`), `e.server`, `e.tool`, `e.call_args`, and
`e.elapsed_ms` localize the failure precisely. An uncaught `ToolError` is
returned as a structured JSON `error` field (`{"tool_error": {...}}`) rather than
a raw traceback, so a partial failure in a batch names the exact call and args.

Every call also accepts a per-call deadline via the `timeout_ms` kwarg:

```python
slow = kubernetes_pods_list(timeout_ms=2000)   # trips a timeout ToolError after 2s
```

### Ergonomics: single calls, allSettled, auto-resolve

For a one-off call there's no boilerplate — the final expression is the result:

```python
github_get_file_contents(owner="o", repo="r", path="README.md")
```

`gather(...)` (alias `settle`) resolves many calls **without raising**, returning
the allSettled shape so one failure never discards the rest of the batch:

```python
result = gather(
    grafana_prod_list_datasources(),
    argocd_prod_list_applications(),
    github_get_me(),
)
# -> [{"ok": True, "value": ...}, {"ok": False, "error": {"kind": ..., "server": ...}}, ...]
```

> **`ok` means "the tool responded," not "the operation succeeded."** A `False`
> entry is a raised `ToolError` (timeout, auth, transport, protocol). Many MCP
> servers report *business-level* failures (a 404, a validation error) as a
> **normal, successful** string result — those come back as `ok: True` with the
> error text as `value`. Inspect `value` when a downstream API error is possible.

Nested `Pending` values inside the returned `result` (in dicts, lists, tuples)
are **deep-resolved automatically** before the result is serialized, so you never
need `json.dumps(..., default=str)` tricks to force resolution. `resolve(x)` is
also available to eagerly resolve a nested structure mid-run.

### Write safety: mutation gating, dry-run, and audit trail

codemcp classifies every tool as read or **mutation** (write). Classification is,
in priority order: the MCP `readOnlyHint`/`destructiveHint` annotation, then an
operator override in `mcp.json` (`tools.<tool>.mutation: true|false`), then a
name-verb heuristic (`create_*`, `*_delete`, `*_merge`, `run_*`, `*_write`, … are
writes; `get_*`, `list_*`, `search_*`, … are reads).

**Enforcement is opt-in and off by default.** Out of the box the agent may call
any tool, including writes, with no ceremony — code-mode behaves like a normal
tool caller. Set **`CODEMCP_ENFORCE_MUTATIONS=true`** (operator env var) to turn
on the pre-flight write gate. When enforcement is on, a mutating call is
**rejected before execution** unless the run explicitly authorizes it, so a write
can't happen without the model naming it.

```jsonc
// Default (enforcement off): reads and writes both just run.
{ "code": "result = github_create_pull_request(owner='o', repo='r', title='t', head='f', base='main')" }

// With CODEMCP_ENFORCE_MUTATIONS=true: enumerate writes in allow_mutations,
// or the run is rejected pre-flight.
{
  "code": "result = github_create_pull_request(owner='o', repo='r', title='t', head='f', base='main')",
  "allow_mutations": ["github_create_pull_request"]
}

// Preview a chain of dependent writes without touching anything (any setting).
{ "code": "...", "dry_run": true }
```

Under `dry_run`, mutating calls are **not** sent upstream — they return a
deterministic stub (with common keys like `id`/`number`/`url` populated) so
dependent logic still runs — while read calls execute normally. The response
reports the `mutations` that *would* have occurred. `dry_run` works regardless of
the enforcement setting.

Responses stay minimal — only fields that carry signal are emitted:

- `result` — always present on success.
- `mutations` — present only when a write (or dry-run write) actually happened:
  the audit trail of write calls, one entry per `{server, tool, ok}`.
- `trace` — present only on **failure**, where it localizes the offending call
  (`{server, tool, ok, elapsed_ms, mutation, kind?}`). On success it's omitted
  because `result` already conveys the outcome.
- `stdout`/`stderr` — omitted when empty.

Sensitive argument values (tokens, passwords, keys, …) are redacted from the
`mutations`/`trace`/error output.

To reclassify individual tools regardless of enforcement, use the per-tool
`mutation` override in `mcp.json` (see the config reference above):
`"tools": { "issue_write": { "mutation": false } }` exempts a tool;
`{ "mutation": true }` forces it to be treated as a write.

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
| `CODEMCP_EXEC_TIMEOUT_MS` | `30000` | Per-`run` execution timeout in milliseconds. A timed-out run returns an error to the model immediately; note that a purely CPU-bound Python loop cannot be force-killed, so the worker thread may keep running in the background until it yields (the worker itself stays responsive to new runs). |
| `CODEMCP_MAX_OUTPUT_BYTES` | `1048576` | Byte cap applied to each model-facing field (`result`, `stdout`, `stderr`) before it is sent back. Oversized `stdout`/`stderr` are truncated with a marker; an oversized `result` is replaced by a compact envelope (`{"_truncated": true, "bytes", "limit", "preview", "hint"}`) so a runaway return or `print` can't blow the token budget. |

### Return shapes

| Variable | Default | Description |
|---|---|---|
| `CODEMCP_LEARN_SHAPES` | `true` | Learn each tool's return shape from its successful calls and use it for strict pre-flight field validation (Tier 1) and a lossy shape in the tool description (Tier 2). On by default; set `false` for byte-identical no-shape behavior. Lazy and bounded; see [Return shapes](#return-shapes-codemcp_learn_shapes-on-by-default). |
| `CODEMCP_SHAPES_IN_DESCRIPTION` | `true` | When shape-learning is on, also append the lossy `# returns: {...}` exemplar to the tool description (Tier 2). Set `false` to run *only* the pre-flight validation tier. No effect when `CODEMCP_LEARN_SHAPES=false`. |
| `CODEMCP_ENFORCE_MUTATIONS` | `false` | Enforce the write-mutation gate. Off by default: the agent may call any tool (including writes) without `allow_mutations`. Set `true` to reject mutating calls pre-flight unless authorized via `allow_mutations` (or previewed with `dry_run`). See [Write safety](#write-safety-mutation-gating-dry-run-and-audit-trail). |

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

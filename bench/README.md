# codemcp bench

A repeatable, verifiable experiment measuring **LLM token usage with and without
codemcp** over identical tasks and an identical toolset.

## Question

Does routing an agent's tool calls through the codemcp gateway (one
`execute_python` tool whose description lists N two-line signatures) use fewer
model tokens than binding the same N upstream tools directly (big per-turn
schema)?

A second question (the **shape-learning experiment**): when codemcp learns and
surfaces each tool's *return shape* (`CODEMCP_LEARN_SHAPES`, the `codemcp_shapes`
arm), does the model stop guessing nested field names — saving round-trips — by
more than the shape lines cost in tokens?

A third question (the **client-lifecycle experiment**, the `codemcp_shapes_relist`
arm): the answer to question two turns out to depend entirely on *client*
behavior, not on MCP or the gateway. So we add an arm whose client is
spec-compliant about `tools/list_changed` — it re-lists and re-binds tools
mid-session — and ask whether *that* client can turn a learned shape into a
within-session saving.

A fourth question (the **field-validation experiment**, the `codemcp_validate`
arm): instead of *showing* the model a shape, use the learned full key structure
to **reject a wrong field access pre-flight** (`result["lgoin"]` → error before
execution, with a suggestion). This rides the worker, not the tool list, so it
works with every client and never touches the prompt cache. Does catching wrong
guesses before they execute cut wasted round-trips?

## Shape-learning experiment — result (corrected)

Four arms (`direct`, `codemcp`, `codemcp_shapes`, `codemcp_shapes_relist`), six
tasks (A–C shallow, D–F nested-field), measured first on Zen and then re-measured
on a **prompt-caching** backend (OpenRouter, same `claude-sonnet-4-6`, caching
on).

**The headline (corrected, and then corrected again): whether the model ever
sees a shape it learns is a property of the CLIENT'S tool-listing lifecycle — not
of MCP, the stdio transport, or the gateway.** Three layers, kept separate:

- **Protocol:** MCP supports mid-session tool changes — that's what
  `notifications/tools/list_changed` is *for*.
- **Model/API:** Anthropic/OpenAI take the full tool schema as a per-request
  parameter, so the model sees whatever tools the client sends on each turn.
  Nothing here blocks a mid-session shape.
- **Client (the actual gate):** does the client re-list tools and re-bind them
  between turns? This is where it's decided.

Two client behaviors, measured head-to-head on the *identical* shape-learning
gateway:

1. **Snapshot-once client (`codemcp_shapes`, what `langchain-mcp-adapters` and
   most clients do).** `tools/list` is called once at startup; `list_changed` is
   **ignored** (the adapter has zero handling for it). The gateway learns a shape
   after a tool's first call and fires `list_changed`, but the client never
   re-reads the description, so the `# returns:` line never reaches the model in
   the session that learned it. Shape-learning is inert here — *not because of
   MCP, but because this client doesn't act on the notification.*

2. **Compliant re-listing client (`codemcp_shapes_relist`, added for this).** It
   installs a `tools/list_changed` handler and, before the next turn, re-runs
   `load_mcp_tools` and re-binds, so the new description reaches the model on the
   very next request. **This works — verified:** the client re-listed mid-session
   on every multi-turn task (B–F: ≥1 re-list/run; the run record's `relisted`
   field counts them). Task A re-lists 0× because it finishes in one tool call —
   the shape is learned but there's no *next* turn to carry it.

**But the compliant client buys no reliable turn reduction — and it does pay a
cache cost.** Even though it sees the shapes:

- Deterministic tasks (A/B/E): `Δturns = 0`, identical turn vectors
  (`[2,2,2]`, `[3,3,3]`, `[3,3,3]`). These already succeed first-try, so there
  was never a shape-guessing retry to remove.
- Noisy tasks (C/D/F): still pure variance — `C` looks better with re-listing
  (`shapes=[9,11,4]` vs `relist=[4,7,4]`), `D` looks worse
  (`[4,2,4]` vs `[3,6,5]`), `F` is a wash. Both arms span 4–11 turns; neither is
  consistently ahead.
- **Cache penalty (newly measured):** re-listing mutates the cached tool-schema
  prefix mid-session, so the next turn must *re-create* the cache. The relist arm
  shows **+4373 `cache_creation` and −4277 `cache_read`** per task vs the
  snapshot-once arm (B/E). This is the real "list_changed busts the cache"
  effect — and it appears **only when a client actually acts on `list_changed`**.

**The prompt-cache question for the snapshot-once client: shapes do NOT bust the
cache.** On the deterministic tasks (A/B/E), `codemcp` and `codemcp_shapes` read
**the same cache** every run (per-task totals, n=3 each):

| task | `codemcp` cache_read | `codemcp_shapes` cache_read |
|---|---|---|
| A | 4278, 4278, 4278 | 4280, 4280, 4280 |
| B | 8556, 8556, 8556 | 8560, 8560, 8560 |
| E | 8556, 8556, 8556 | 8560, 8560, 8560 |

The +2/+4 is the per-run cache nonce, not a shape effect. The description is
frozen at session start, so the cached prefix is never mutated — *for a client
that doesn't re-list*. (A client that re-lists pays the +4373 cache_creation
shown above. So the cache cost isn't "free in general" — it's free precisely
because the dominant client never picks the shape up.)

The noisy tasks (C/D/F) show large per-run turn spread in *every* arm
(`C codemcp=[8,9,4]`, `F shapes=[5,3,7]`) with none consistently ahead — and the
apparent shape "win" lands on **different** tasks across runs (D/F on one Zen run,
C on the caching run), the signature of variance rather than a real effect.

**Net:** there is no measured within-session benefit to shape-learning in this
bench, under *either* client. The snapshot-once client can't see shapes (so they
cost nothing); the compliant client sees them but extracts no reliable turn
saving while paying a prompt-cache re-creation cost. The honest conclusion is
that surfacing shapes through the tool *description* is the wrong channel — it
either isn't read (snapshot-once) or busts the cache when it is (re-listing). A
shape delivered inline in the tool *result* (which every client always feeds back
to the model, with no re-list and no prefix mutation) is the design that could
actually pay off; that's the recommended next experiment.

**Where shapes *do* work: the shared, multi-session gateway.** With a long-lived
`codemcp start` HTTP gateway serving several sessions, a shape learned in
session 1 **is** present at session 2's `tools/list`. *Verified:* fresh session 2
against the same shared gateway sees `# returns: {...}`. So the value is real but
**cross-session**, not within-session.

### Verdict

- **Snapshot-once client (the dominant case — opencode, `langchain-mcp-adapters`,
  this bench's default): shape-learning is effectively a no-op** — the client
  never re-lists, so the model never sees the shape the session learned. No turn
  benefit, but also **no prompt-cache downside** (the cached prefix is never
  mutated).
- **Compliant re-listing client (`codemcp_shapes_relist`, measured here): sees
  the shape mid-session, but extracts no reliable turn saving** (deterministic
  tasks unchanged; noisy tasks remain variance-dominated) **and pays a
  prompt-cache re-creation cost** (+~4373 `cache_creation`, −~4277 `cache_read`
  per task, because re-listing mutates the cached tool-schema prefix).
- **Shared HTTP gateway with multiple sessions: shapes reach later sessions** and
  can help — but this bench doesn't exercise that topology, so the magnitude is
  unmeasured here.

**Recommendation: keep `CODEMCP_LEARN_SHAPES` off by default.** Under the
dominant snapshot-once client it does nothing for the model while doing
per-first-call work; under a compliant client it busts the prompt cache without a
measurable turn payoff. The honest follow-ups that would justify turning it on:
(a) a multi-session shared-gateway bench, and (b) — the more promising one —
surfacing shapes through a channel that needs no re-list and mutates no cached
prefix: **inline in the tool result**, which every client always feeds back to
the model. That avoids both failure modes this experiment exposed.

## Field-validation experiment — result

The `codemcp_validate` arm takes the **other** path to the same goal: instead of
*showing* the model a shape, it uses the learned full key structure to **reject a
wrong field access before execution** (`result["lgoin"]` → pre-flight error with
a `did you mean "login"?` hint). This rides the worker, so it works with the
default snapshot-once client and never touches the prompt cache. (Run with
`CODEMCP_SHAPES_IN_DESCRIPTION=false`, so it isolates validation from the
description tier.)

**What the mechanism does (verified e2e, separately from the bench):** a real
wrong-field guess *is* rejected pre-flight against a live GitHub result — first
call learns the keyset, `github_get_me()["lgoin"]` is refused with the suggestion,
`["login"]` runs. The plumbing works end-to-end.

**What the bench measured (90 runs, OpenRouter `claude-sonnet-4.6`, caching on):**

- **No prompt-cache impact — confirmed.** `Δcache_creation = +2` (the nonce) on
  *every* task vs plain `codemcp`. Unlike the re-listing arm, validation leaves
  the description untouched, so nothing is re-created. This is the design's main
  promise and it holds exactly.
- **No measured turn benefit — because the tasks didn't provoke wrong guesses.**
  **Zero pre-flight field rejections fired across all 90 runs.** On these GitHub
  tasks the model nearly always names fields correctly first try (`login`,
  `number`, …), so the validator never had a bad guess to catch. The turn deltas
  that *do* appear are variance, not validation: deterministic tasks (A/B/E) are
  identical (`[2,2,2]`/`[3,3,3]`/`[3,3,3]`); on the noisy tasks the spread swings
  both ways (D `codemcp=[4,9,5]` vs `validate=[2,2,3]` looks like a win, but
  C `codemcp=[6,5,4]` vs `validate=[7,7,8]` looks like a loss) and tracks
  tool-call outliers (D/codemcp drew an 8-call flailing run) — with **no
  rejection events** to attribute any of it to the feature.

**Net:** the validation tier is the *right shape* of solution — zero cache cost,
client-independent, never in the model's context, and proven to catch real
mistakes — but this task set doesn't generate enough wrong-field guesses to show
a turn payoff above the noise floor. The honest follow-up is a task set that
*forces* obscure nested-field access (deep `commit.author.*`, `*.reactions.*`,
union-typed fields) where a strong model genuinely guesses wrong, with enough
repeats to clear variance. The feature stays off by default; its value is a
correctness/latency safety net for hard shapes, not a measured win on easy ones.

## Design

Five **arms**, identical except for how the GitHub MCP toolset reaches the model,
how the client reacts to mid-session tool-list changes, and whether the gateway
strictly validates field access:

| arm | what the model sees | what runs the tools |
|---|---|---|
| `direct` | all ~45 GitHub MCP tools, bound directly | LangGraph `ToolNode` calls the GitHub MCP server |
| `codemcp` | one `execute_python` tool (description = 45 two-line sigs) | agent writes Python; codemcp gateway routes SDK calls to the same GitHub MCP server |
| `codemcp_shapes` | same as `codemcp`, plus a learned `# returns: {...}` line per tool *after its first call* — **but the snapshot-once client never re-reads it** | identical gateway with `CODEMCP_LEARN_SHAPES=true` |
| `codemcp_shapes_relist` | **same gateway as `codemcp_shapes`, but a spec-compliant client** that handles `tools/list_changed`, re-lists, and re-binds tools mid-session so the learned `# returns:` line reaches the model on the next turn | identical gateway with `CODEMCP_LEARN_SHAPES=true` |
| `codemcp_validate` | **same as `codemcp`** (no shape in the description) — but the gateway ships the full learned key structure to the worker, which **rejects wrong field access pre-flight** | gateway with `CODEMCP_LEARN_SHAPES=true` + `CODEMCP_SHAPES_IN_DESCRIPTION=false` |

The `codemcp_shapes` / `codemcp_shapes_relist` pair differs **only in client
behavior**, isolating "does the client act on `list_changed`" from the
gateway/transport. The `codemcp_validate` arm isolates the validation tier from
the description tier (it has the field check but no description shape). The
`relisted` field in each
run record counts how many times that client re-listed mid-session (0 for the
snapshot-once arm by construction).

All arms bind the **same** upstream server (`ghcr.io/github/github-mcp-server`,
github entry sourced from the user's codemcp `mcp.json`) and run the **same**
model on the **same** endpoint.

### Model + endpoint

Two providers, selected with `BENCH_PROVIDER` (default `zen`):

- `zen` — **OpenCode Zen** Anthropic-compatible endpoint
  `https://opencode.ai/zen/v1/messages`, model `claude-sonnet-4-6`,
  `temperature=0`. Key from `~/.local/share/opencode/auth.json` (`opencode`).
  Prompt caching was never observed here (`cache_read` always 0).
- `openrouter` — **OpenRouter Anthropic-native** endpoint with the **same**
  `claude-sonnet-4.6`, prompt caching **enabled** via `cache_control: ephemeral`
  on the tool-schema + system prefix. This is the arm that can detect a
  shape-driven cache bust. A per-run nonce makes each run's cached prefix unique
  so the provider's ~5-min ephemeral cache isn't shared across runs (otherwise
  one run warms the cache for the next and contaminates the measurement). Key
  from `OPENROUTER_API_KEY` in `bench/.env`. Run with
  `BENCH_PROVIDER=openrouter`.

### Tasks (over the GitHub user `skymoore`)

| id | name | tool calls expected | answer shape |
|---|---|---|---|
| A | repo_count | 1 | `{"repo_count": int}` |
| B | most_starred_latest_commit | 2 | `{"repo": str, "stars": int, "latest_commit_message": str}` |
| C | most_issues_readme | 2 | `{"repo": str, "open_issues": int, "has_readme": bool}` |
| D | most_starred_owner_and_language | 1–2 | `{"repo": str, "owner_type": str, "language": str}` |
| E | latest_commit_author | 2 | `{"repo": str, "author_name": str, "commit_date": str}` |
| F | most_issues_first_issue_author | 2 | `{"repo": str, "issue_number": int, "issue_author": str}` |

Tasks **D–F require indexing into nested fields** of returned objects whose
names are not obvious from the tool signature (`owner.type`, `commit.author.name`,
`user.login`). They are where shape-guessing causes retries, isolating the value
of the `codemcp_shapes` arm.

Tasks let the agent take as many turns as it needs and answer in natural prose
(no strict-JSON gate). Correctness is reviewed **manually** against
`ground_truth.json`: `analyze.py` prints every final answer next to the truth in
a `## manual review` section, plus a best-effort auto-flag (JSON / `key: value`
extraction). The auto-flag is a convenience, not authoritative.

### Measurement (authoritative, not estimated)

Token counts come from Anthropic's `usage` block, surfaced by `langchain-anthropic`
as `AIMessage.usage_metadata`, summed per run:

- `input_tokens`, `output_tokens`
- `cache_read_input_tokens`, `cache_creation_input_tokens` (reported separately —
  Anthropic caches tool schemas, so this is where the direct arm may recover
  cost on turns > 1)

Also recorded: model turns, tool calls, wall-clock seconds, the final answer.

### Repeats

LLMs are stochastic even at `temperature=0` (minor provider-side nondeterminism).
Default: **3 repeats** per (task, arm) → 18 runs. Means + stddev reported.

## Files

```
bench/
  pyproject.toml        pinned deps (uv-managed, package=false)
  .env                  GITHUB_TOKEN (git-ignored)
  .gitignore
  mcp.github.json       bench-local codemcp config; only `github` enabled;
                        uses {env:GITHUB_TOKEN} interpolation
  tasks.py              3 task definitions (prompts + answer schemas)
  configs.py            builds direct + codemcp MCP client configs; loads .env;
                        resolves {env:...}; reads Zen key
  ground_truth.py       computes ground_truth.json via direct GitHub tool calls
  harness.py            builds a LangGraph agent per arm; captures per-turn usage
  runner.py             runs the task x arm x repeat matrix -> results/runs.jsonl
  analyze.py            scores + aggregates -> results/summary.{md,csv}
  results/              runs.jsonl, summary.md, summary.csv (git-ignored)
  ground_truth.json     computed truth (git-ignored, regeneratable)
```

## Run

From this directory:

```bash
# 1. install deps into a local venv
uv sync

# 2. (re)generate ground truth — calls the GitHub MCP server directly
uv run python ground_truth.py

# 3. smoke test: 1 repeat, all tasks/arms (2 x 3 = 6 runs) — checks connectivity
uv run python runner.py --smoke

# 4. analyze
uv run python analyze.py

# 5. full bench: 3 repeats (18 runs)
uv run python runner.py --reset --repeats 3
uv run python analyze.py

# inspect
cat results/summary.md
```

Overrides:
- `BENCH_MODEL=claude-sonnet-4-6` — change the Zen model.
- `CODEMCP_BIN=/path/to/codemcp` — pin the gateway binary.
- `uv run python runner.py --task A --arm direct --repeats 5` — subset.

## Fairness notes / caveats

- **Identical toolset.** Both arms expose the same GitHub server. The direct arm
  binds ~45 LangChain tools; the codemcp arm binds 1 tool whose description
  carries 45 two-line signatures. That description size difference is exactly
  what's being measured.
- **Per-run isolation.** Each run builds a fresh MCP client + agent; the codemcp
  arm spawns a fresh gateway subprocess per run. No shared state across runs.
- **Prompt caching is real and reported separately.** Anthropic caches the tool
  schema block; on multi-turn runs the direct arm's repeat-turn input may be
  mostly `cache_read`. The summary splits `cache_read` vs `cache_creation` so
  the comparison is honest, not hidden behind a single "input" number.
- **Ground truth drifts with live data.** `ground_truth.py` re-queries GitHub,
  so if `skymoore`'s repos/issues change, truth changes with them — the bench
  stays correct but results aren't bit-identical across days. Regenerate before
  each bench.
- **Errors excluded from token stats** but listed in the report.
- **Manual correctness review.** `analyze.py` shows each run's full final
  answer next to ground truth; a best-effort auto-flag (JSON or `key: value`
  extraction) is included but not authoritative. The agent may take as many
  turns as it needs.
- The GitHub token lives in `.env` (git-ignored); `mcp.github.json` uses
  `{env:GITHUB_TOKEN}`. Never committed.

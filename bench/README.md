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

## Shape-learning experiment — result (corrected)

Three arms (`direct`, `codemcp`, `codemcp_shapes`), six tasks (A–C shallow,
D–F nested-field), measured first on Zen and then re-measured on a
**prompt-caching** backend (OpenRouter, same `claude-sonnet-4-6`, caching on).

**The headline correction: within a single agent session, shape-learning is
inert — the model never sees the shapes it learns.** Two facts establish this:

1. **MCP clients don't re-list tools mid-session.** `langchain-mcp-adapters`
   (and most clients) call `tools/list` once at startup and **ignore**
   `tools/list_changed`. The gateway learns a shape after a tool's first call
   and fires `list_changed`, but the client never re-reads the description, so
   the `# returns:` line never reaches the model in the session that learned it.
   *Verified directly:* a fresh `tools/list` shows the shape, but the bound tool
   object the agent holds does not.
2. **The earlier C/F "wins" were variance, not shapes.** With the model unable
   to see shapes mid-session, shapes cannot have caused the reduction. The runs
   confirm it: plain `codemcp` drew **one flailing 6-turn outlier** per task
   (it confused itself, unrelated to shapes); with only 3 repeats that single
   outlier moved the mean by the ~1.3 turns previously attributed to shapes.
   `codemcp_shapes` simply didn't draw a bad run.

**The prompt-cache question (the reason for the OpenRouter re-run): shapes do
NOT bust the cache.** On the deterministic single-path tasks (A/B/E), the two
arms read **the same cache** every run (per-task totals, n=3 each):

| task | `codemcp` cache_read | `codemcp_shapes` cache_read |
|---|---|---|
| A | 4278, 4278, 4278 | 4280, 4280, 4280 |
| B | 8556, 8556, 8556 | 8560, 8560, 8560 |
| E | 8556, 8556, 8556 | 8560, 8560, 8560 |

The +2/+4 is the per-run cache nonce, not a shape effect. Because the description
is frozen at session start, the cached tool-schema prefix is never mutated
mid-session, so the cache is not invalidated. The earlier worry ("mid-session
`list_changed` busts prompt caching") **does not occur** with real-world clients.

The noisy tasks (C/D/F) show large per-run turn spread in *both* arms
(`C codemcp=[8,9,4]`, `F shapes=[5,3,7]`) with neither consistently ahead — and
the apparent shape "win" lands on **different** tasks than the Zen run (D/F here,
C/F there), the signature of variance rather than a real effect.

**Where shapes *do* work: the shared, multi-session gateway.** With a long-lived
`codemcp start` HTTP gateway serving several sessions, a shape learned in
session 1 **is** present at session 2's `tools/list`. *Verified:* fresh session 2
against the same shared gateway sees `# returns: {...}`. So the value is real but
**cross-session**, not within-session.

### Verdict

- **Default stdio-per-session topology (opencode, this bench): shape-learning is
  effectively a no-op** — each session gets a fresh gateway, learns shapes the
  session never sees, and discards them on exit.
- **Shared HTTP gateway with multiple sessions: shapes reach later sessions** and
  can help — but this bench doesn't exercise that topology, so the magnitude is
  unmeasured here.
- **No prompt-cache downside either way.**

**Recommendation: keep `CODEMCP_LEARN_SHAPES` off by default.** Not because it's
risky (it isn't), but because in the dominant single-session topology it does
nothing for the model while still doing per-first-call work. The honest
follow-ups that would justify turning it on: (a) a multi-session shared-gateway
bench, and (b) clients that honor `tools/list_changed` (or surfacing shapes
through a channel that doesn't depend on re-listing — e.g. inline in the tool
*result*, which every client always sees).

## Design

Two **arms**, identical except for how the GitHub MCP toolset reaches the model:

| arm | what the model sees | what runs the tools |
|---|---|---|
| `direct` | all ~45 GitHub MCP tools, bound directly | LangGraph `ToolNode` calls the GitHub MCP server |
| `codemcp` | one `execute_python` tool (description = 45 two-line sigs) | agent writes Python; codemcp gateway routes SDK calls to the same GitHub MCP server |
| `codemcp_shapes` | same as `codemcp`, plus a learned `# returns: {...}` line per tool *after its first call* | identical gateway with `CODEMCP_LEARN_SHAPES=true` |

Both arms bind the **same** upstream server (`ghcr.io/github/github-mcp-server`,
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

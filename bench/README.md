# codemcp bench

A repeatable, verifiable experiment measuring **LLM token usage with and without
codemcp** over identical tasks and an identical toolset.

## Question

Does routing an agent's tool calls through the codemcp gateway (one
`execute_python` tool whose description lists N two-line signatures) use fewer
model tokens than binding the same N upstream tools directly (big per-turn
schema)?

## Design

Two **arms**, identical except for how the GitHub MCP toolset reaches the model:

| arm | what the model sees | what runs the tools |
|---|---|---|
| `direct` | all ~45 GitHub MCP tools, bound directly | LangGraph `ToolNode` calls the GitHub MCP server |
| `codemcp` | one `execute_python` tool (description = 45 two-line sigs) | agent writes Python; codemcp gateway routes SDK calls to the same GitHub MCP server |

Both arms bind the **same** upstream server (`ghcr.io/github/github-mcp-server`,
github entry sourced from the user's codemcp `mcp.json`) and run the **same**
model on the **same** endpoint.

### Model + endpoint

- Inference: **OpenCode Zen** Anthropic-compatible endpoint
  `https://opencode.ai/zen/v1/messages`, model `claude-sonnet-4-6`, `temperature=0`.
- API key: read from `~/.local/share/opencode/auth.json` (`opencode` key).

### Tasks (over the GitHub user `skymoore`)

| id | name | tool calls expected | answer shape |
|---|---|---|---|
| A | repo_count | 1 | `{"repo_count": int}` |
| B | most_starred_latest_commit | 2 | `{"repo": str, "stars": int, "latest_commit_message": str}` |
| C | most_issues_readme | 2 | `{"repo": str, "open_issues": int, "has_readme": bool}` |

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

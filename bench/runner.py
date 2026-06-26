"""Orchestrate the bench: run the task x arm x repeat matrix, append each run
to results/runs.jsonl, then hand off to analyze.py.

Usage:
    uv run python runner.py --repeats 3        # full bench: 3 tasks x 2 arms x 3 = 18 runs
    uv run python runner.py --smoke            # 1 task x 2 arms x 1 = 2 runs (connectivity check)
    uv run python runner.py --task A --arm direct --repeats 1

Each run rebuilds a fresh MCP client + agent so there is no shared state
between runs (codemcp arm gets a fresh gateway subprocess each time). The LLM
client is rebuilt per run too, so temperature=0 repeatability is per-config.

Run records are appended (not overwritten) so you can accumulate data across
bench invocations; pass --reset to clear runs.jsonl first. analyze.py reads
the whole file.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
import time
from pathlib import Path
from typing import Any

from configs import ARMS, active_model, load_api_key
from harness import build_llm, run_one
from tasks import TASKS

RESULTS_DIR = Path(__file__).resolve().parent / "results"
RUNS_PATH = RESULTS_DIR / "runs.jsonl"


def _index_tasks(ids: list[str] | None) -> list[dict[str, Any]]:
    if not ids:
        return list(TASKS)
    by_id = {t["id"]: t for t in TASKS}
    out = []
    for i in ids:
        if i not in by_id:
            raise SystemExit(f"unknown task id {i!r}; have {list(by_id)}")
        out.append(by_id[i])
    return out


def _index_arms(arms: list[str] | None) -> list[str]:
    if not arms:
        return list(ARMS)
    bad = [a for a in arms if a not in ARMS]
    if bad:
        raise SystemExit(f"unknown arm(s) {bad}; have {list(ARMS)}")
    return arms


async def _run_matrix(
    tasks: list[dict[str, Any]],
    arms: list[str],
    repeats: int,
) -> list[dict[str, Any]]:
    api_key = load_api_key()
    records: list[dict[str, Any]] = []
    total = len(tasks) * len(arms) * repeats
    done = 0
    for r in range(1, repeats + 1):
        for task in tasks:
            for arm in arms:
                done += 1
                label = f"r{r}/{task['id']}/{arm}"
                print(f"[{done}/{total}] {label}", flush=True)
                # Fresh LLM + fresh MCP client per run (no cross-run state).
                llm = build_llm(api_key)
                t0 = time.perf_counter()
                try:
                    rec = await run_one(arm, task, llm, label=label)
                except Exception as e:  # noqa: BLE001
                    rec = {
                        "arm": arm,
                        "task_id": task["id"],
                        "task_name": task["name"],
                        "model": active_model(),
                        "label": label,
                        "ok": False,
                        "error": f"{type(e).__name__}: {e}",
                        "totals": {},
                        "usage_per_turn": [],
                        "num_turns": 0,
                        "tool_calls": 0,
                        "tool_results": 0,
                        "num_tools_bound": 0,
                        "wall_seconds": round(time.perf_counter() - t0, 3),
                        "final_answer": "",
                    }
                rec["repeat"] = r
                rec["started_at"] = time.strftime("%Y-%m-%dT%H:%M:%S%z")
                records.append(rec)
                _append_run(rec)
                _print_rec(rec)
    return records


def _append_run(rec: dict[str, Any]) -> None:
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    with open(RUNS_PATH, "a") as f:
        f.write(json.dumps(rec, sort_keys=True) + "\n")


def _print_rec(rec: dict[str, Any]) -> None:
    t = rec.get("totals", {})
    status = "OK " if rec.get("ok") else "ERR"
    ans = (rec.get("final_answer") or "").replace("\n", " ")[:70]
    print(
        f"    {status} turns={rec.get('num_turns')} tools={rec.get('tool_calls')} "
        f"in={t.get('input', 0)} out={t.get('output', 0)} "
        f"cache_r={t.get('cache_read', 0)} cache_w={t.get('cache_creation', 0)} "
        f"wall={rec.get('wall_seconds')}s ans={ans!r}",
        flush=True,
    )
    if not rec.get("ok"):
        print(f"    error: {rec.get('error')}", flush=True)


def main() -> None:
    ap = argparse.ArgumentParser(description="Run the codemcp token bench.")
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--task", nargs="*", help="task id(s) to run (default: all)")
    ap.add_argument("--arm", nargs="*", help="arm(s) to run (default: all)")
    ap.add_argument("--smoke", action="store_true", help="1 repeat, all tasks/arms")
    ap.add_argument("--reset", action="store_true", help="clear runs.jsonl first")
    args = ap.parse_args()

    if args.reset and RUNS_PATH.exists():
        RUNS_PATH.unlink()

    tasks = _index_tasks(args.task)
    arms = _index_arms(args.arm)
    repeats = 1 if args.smoke else args.repeats

    print(
        f"bench: model={active_model()} tasks={[t['id'] for t in tasks]} "
        f"arms={arms} repeats={repeats} -> {len(tasks)*len(arms)*repeats} runs",
        flush=True,
    )
    print(f"appending to {RUNS_PATH}\n", flush=True)

    records = asyncio.run(_run_matrix(tasks, arms, repeats))
    ok = sum(1 for r in records if r.get("ok"))
    print(f"\ndone: {ok}/{len(records)} runs ok", flush=True)
    if ok < len(records):
        sys.exit(1)


if __name__ == "__main__":
    main()

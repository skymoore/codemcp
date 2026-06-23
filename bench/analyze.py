"""Analyze results/runs.jsonl against ground_truth.json.

Scores each run's final answer for correctness (deterministic JSON parse +
field-by-field equality, no LLM judging), then computes per-(task,arm) token
statistics and the direct-vs-codemcp deltas, writing:

  results/summary.md   - human-readable report (tables + deltas)
  results/summary.csv  - one row per (task, arm) with means + deltas

Re-runnable any time against the accumulated runs.jsonl.
"""

from __future__ import annotations

import csv
import json
import re
import statistics
import sys
from pathlib import Path
from typing import Any

BENCH_DIR = Path(__file__).resolve().parent
RESULTS_DIR = BENCH_DIR / "results"
RUNS_PATH = RESULTS_DIR / "runs.jsonl"
TRUTH_PATH = BENCH_DIR / "ground_truth.json"
SUMMARY_MD = RESULTS_DIR / "summary.md"
SUMMARY_CSV = RESULTS_DIR / "summary.csv"

from tasks import TASKS  # noqa: E402

TASK_BY_ID = {t["id"]: t for t in TASKS}
ARMS = ("direct", "codemcp")


def _load_runs() -> list[dict[str, Any]]:
    if not RUNS_PATH.exists():
        sys.exit(f"no runs at {RUNS_PATH}; run `uv run python runner.py` first")
    out = []
    for line in RUNS_PATH.read_text().splitlines():
        line = line.strip()
        if line:
            out.append(json.loads(line))
    return out


def _load_truth() -> dict[str, Any]:
    if not TRUTH_PATH.exists():
        sys.exit(
            f"no ground truth at {TRUTH_PATH}; run `uv run python ground_truth.py` first"
        )
    return json.loads(TRUTH_PATH.read_text())


def _extract_json(text: str) -> dict[str, Any] | None:
    """Best-effort: pull a JSON object out of the answer, if present."""
    if not text:
        return None
    s = text.strip()
    try:
        v = json.loads(s)
        return v if isinstance(v, dict) else None
    except json.JSONDecodeError:
        pass
    m = re.search(r"\{.*\}", s, re.DOTALL)
    if m:
        try:
            v = json.loads(m.group(0))
            return v if isinstance(v, dict) else None
        except json.JSONDecodeError:
            return None
    return None


def _extract_kv(text: str, keys: list[str]) -> dict[str, Any]:
    """Best-effort: scan prose for `key: value` / `key = value` lines.

    The prompts ask the agent to name each value on its own line as
    `key: value`, so this catches the common case when the model answers in
    prose rather than JSON. All matching is best-effort — the human reviews.
    """
    out: dict[str, Any] = {}
    if not text:
        return out
    for key in keys:
        pat = rf"(?i)^\s*[-*]?\s*`?{re.escape(key)}`?\s*[:=]\s*`?([^`\n]+?)`?\s*$"
        m = re.search(pat, text, re.MULTILINE)
        if m:
            out[key] = m.group(1).strip().strip(".,")
    return out


def _coerce(value: Any, typ: type) -> Any:
    try:
        if typ is bool:
            if isinstance(value, bool):
                return value
            if isinstance(value, str):
                return value.strip().lower() in ("true", "1", "yes")
            return bool(value)
        if typ is int:
            # strip commas / non-digit noise around an integer
            if isinstance(value, str):
                m = re.search(r"-?\d[\d,]*", value)
                if m:
                    return int(m.group(0).replace(",", ""))
                return None
            return int(value)
        if typ is str:
            return str(value).strip()
    except (TypeError, ValueError):
        return None
    return value


def _score_answer(task_id: str, answer_text: str, truth: dict[str, Any]) -> dict[str, Any]:
    """Best-effort auto-flag. NOT authoritative — humans review final_answer.

    Extracts values from JSON if present, else from `key: value` prose lines,
    and compares field-by-field. Returns {auto_correct, found, fields}. A run
    with no extractable values is flagged auto_correct=False but is still kept;
    the manual-review section shows the raw answer regardless.
    """
    task = TASK_BY_ID[task_id]
    expected = truth[task_id]
    keys = list(task["answer_keys"].keys())
    parsed = _extract_json(answer_text) or {}
    kv = _extract_kv(answer_text, keys)
    merged = {**kv, **parsed}  # JSON wins if both present
    fields: dict[str, Any] = {}
    found_any = False
    all_ok = True
    for key, typ in task["answer_keys"].items():
        if key not in merged:
            fields[key] = {"got": None, "expected": expected[key], "ok": False, "found": False}
            all_ok = False
            continue
        found_any = True
        got = _coerce(merged[key], typ)
        ok = got == expected[key]
        if not ok:
            all_ok = False
        fields[key] = {"got": got, "expected": expected[key], "ok": ok, "found": True}
    return {
        "auto_correct": all_ok and found_any,
        "found_any": found_any,
        "fields": fields,
    }


def _mean(xs: list[float]) -> float:
    return statistics.fmean(xs) if xs else 0.0


def _stdev(xs: list[float]) -> float:
    return statistics.stdev(xs) if len(xs) >= 2 else 0.0


def _token_fields(rec: dict[str, Any]) -> dict[str, int]:
    t = rec.get("totals") or {}
    return {
        "input": int(t.get("input", 0) or 0),
        "output": int(t.get("output", 0) or 0),
        "cache_read": int(t.get("cache_read", 0) or 0),
        "cache_creation": int(t.get("cache_creation", 0) or 0),
    }


def analyze() -> dict[str, Any]:
    runs = _load_runs()
    truth = _load_truth()

    # attach best-effort score to each run (NOT authoritative; humans review)
    for rec in runs:
        text = rec.get("final_answer") or ""
        rec["_answer_text"] = text
        rec["_score"] = _score_answer(rec["task_id"], text, truth)

    # group by (task_id, arm)
    groups: dict[tuple[str, str], list[dict[str, Any]]] = {}
    for rec in runs:
        if not rec.get("ok"):
            continue  # exclude errored runs from token stats (kept for report)
        groups.setdefault((rec["task_id"], rec["arm"]), []).append(rec)

    rows: list[dict[str, Any]] = []
    for task in TASKS:
        for arm in ARMS:
            grp = groups.get((task["id"], arm), [])
            n = len(grp)
            auto_correct = sum(1 for r in grp if r["_score"]["auto_correct"])
            if n == 0:
                rows.append({
                    "task_id": task["id"], "task_name": task["name"], "arm": arm,
                    "n": 0, "auto_correct": 0, "auto_accuracy": None,
                })
                continue
            ins = [_token_fields(r)["input"] for r in grp]
            outs = [_token_fields(r)["output"] for r in grp]
            cr = [_token_fields(r)["cache_read"] for r in grp]
            cc = [_token_fields(r)["cache_creation"] for r in grp]
            turns = [r.get("num_turns", 0) for r in grp]
            tcalls = [r.get("tool_calls", 0) for r in grp]
            walls = [r.get("wall_seconds", 0.0) for r in grp]
            rows.append({
                "task_id": task["id"], "task_name": task["name"], "arm": arm,
                "n": n, "auto_correct": auto_correct, "auto_accuracy": auto_correct / n,
                "input_mean": _mean(ins), "input_sd": _stdev(ins),
                "output_mean": _mean(outs), "output_sd": _stdev(outs),
                "cache_read_mean": _mean(cr), "cache_read_sd": _stdev(cr),
                "cache_creation_mean": _mean(cc), "cache_creation_sd": _stdev(cc),
                "turns_mean": _mean(turns), "tool_calls_mean": _mean(tcalls),
                "wall_mean": _mean(walls),
            })

    return {"runs": runs, "truth": truth, "rows": rows}


def _write_csv(rows: list[dict[str, Any]]) -> None:
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    fields = [
        "task_id", "task_name", "arm", "n", "auto_correct", "auto_accuracy",
        "input_mean", "input_sd", "output_mean", "output_sd",
        "cache_read_mean", "cache_read_sd",
        "cache_creation_mean", "cache_creation_sd",
        "turns_mean", "tool_calls_mean", "wall_mean",
    ]
    with open(SUMMARY_CSV, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=fields)
        w.writeheader()
        for r in rows:
            w.writerow({k: r.get(k, "") for k in fields})


def _write_md(data: dict[str, Any]) -> None:
    runs = data["runs"]
    truth = data["truth"]
    rows = data["rows"]
    by_key = {(r["task_id"], r["arm"]): r for r in rows}
    lines: list[str] = []
    lines.append("# codemcp bench — results\n")
    lines.append(f"- runs analyzed: {len(runs)}")
    ok_runs = [r for r in runs if r.get("ok")]
    err_runs = [r for r in runs if not r.get("ok")]
    lines.append(f"- ok: {len(ok_runs)}  errored: {len(err_runs)}")
    lines.append(f"- ground truth: `{TRUTH_PATH.name}`")
    lines.append("- correctness column is **best-effort auto-flag** (JSON or `key: value` "
                 "extraction vs ground truth). NOT authoritative — review the "
                 "`## manual review` section.\n")

    lines.append("## ground truth\n")
    lines.append("```json")
    lines.append(json.dumps(truth, indent=2, sort_keys=True))
    lines.append("```\n")

    lines.append("## per (task, arm) summary\n")
    hdr = (
        "| task | arm | n | auto | in | out | cache_r | cache_w | turns | tools | wall(s) |"
    )
    sep = "|---|---|---|---|---|---|---|---|---|---|---|"
    lines.append(hdr)
    lines.append(sep)
    for task in TASKS:
        for arm in ARMS:
            r = by_key.get((task["id"], arm))
            if not r or r.get("n", 0) == 0:
                lines.append(f"| {task['id']} | {arm} | 0 | – | – | – | – | – | – | – | – |")
                continue
            def f(v, sd=None):
                if v is None:
                    return "–"
                s = f"{v:.0f}" if isinstance(v, (int, float)) else str(v)
                if sd is not None and sd > 0:
                    s += f"±{sd:.0f}"
                return s
            lines.append(
                f"| {task['id']} | {arm} | {r['n']} | "
                f"{r['auto_accuracy']*100:.0f}% | "
                f"{f(r.get('input_mean'), r.get('input_sd'))} | "
                f"{f(r.get('output_mean'), r.get('output_sd'))} | "
                f"{f(r.get('cache_read_mean'), r.get('cache_read_sd'))} | "
                f"{f(r.get('cache_creation_mean'), r.get('cache_creation_sd'))} | "
                f"{r.get('turns_mean',0):.1f} | "
                f"{r.get('tool_calls_mean',0):.1f} | "
                f"{r.get('wall_mean',0):.1f} |"
            )
    lines.append("")

    lines.append("## deltas (codemcp vs direct)\n")
    lines.append("negative = codemcp uses fewer; positive = codemcp uses more.\n")
    lines.append("| task | Δinput | Δoutput | Δcache_r | Δcache_w | Δturns | Δtools | Δwall(s) |")
    lines.append("|---|---|---|---|---|---|---|---|")
    for task in TASKS:
        d = by_key.get((task["id"], "direct"))
        c = by_key.get((task["id"], "codemcp"))
        if not d or not c or d.get("n", 0) == 0 or c.get("n", 0) == 0:
            lines.append(f"| {task['id']} | – | – | – | – | – | – | – |")
            continue

        def delta(k):
            return (c.get(k, 0) or 0) - (d.get(k, 0) or 0)

        lines.append(
            f"| {task['id']} | "
            f"{delta('input_mean'):+.0f} | "
            f"{delta('output_mean'):+.0f} | "
            f"{delta('cache_read_mean'):+.0f} | "
            f"{delta('cache_creation_mean'):+.0f} | "
            f"{delta('turns_mean'):+.2f} | "
            f"{delta('tool_calls_mean'):+.2f} | "
            f"{delta('wall_mean'):+.2f} |"
        )
    lines.append("")

    if err_runs:
        lines.append("## errored runs\n")
        for r in err_runs:
            lines.append(
                f"- {r.get('label')} ({r['task_id']}/{r['arm']}): {r.get('error')}"
            )
        lines.append("")

    lines.append("## per-run detail\n")
    lines.append("| label | task | arm | auto | turns | tools | in | out | wall(s) |")
    lines.append("|---|---|---|---|---|---|---|---|---|")
    for r in runs:
        sc = r.get("_score", {})
        mark = "✓" if sc.get("auto_correct") else "✗"
        t = r.get("totals", {}) or {}
        lines.append(
            f"| {r.get('label','')} | {r['task_id']} | {r['arm']} | {mark} | "
            f"{r.get('num_turns',0)} | {r.get('tool_calls',0)} | "
            f"{t.get('input',0)} | {t.get('output',0)} | {r.get('wall_seconds',0)} |"
        )
    lines.append("")

    lines.append("## manual review\n")
    lines.append("Full final answers next to ground truth. The `auto` flag is a "
                 "best-effort guess — verify each by eye.\n")
    for task in TASKS:
        lines.append(f"### task {task['id']} — {task['name']}\n")
        lines.append(f"**truth:** `{json.dumps(truth[task['id']], sort_keys=True)}`\n")
        arm_runs = [r for r in runs if r["task_id"] == task["id"]]
        if not arm_runs:
            lines.append("_(no runs)_\n")
            continue
        for r in arm_runs:
            sc = r.get("_score", {})
            mark = "auto✓" if sc.get("auto_correct") else "auto✗"
            t = r.get("totals", {}) or {}
            err = r.get("error") if not r.get("ok") else None
            lines.append(
                f"**{r.get('label')}** [{r['arm']}] — {mark} | "
                f"turns={r.get('num_turns',0)} tools={r.get('tool_calls',0)} | "
                f"in={t.get('input',0)} out={t.get('output',0)} "
                f"cache_r={t.get('cache_read',0)} cache_w={t.get('cache_creation',0)} | "
                f"wall={r.get('wall_seconds',0)}s"
            )
            if err:
                lines.append(f"\n> ERROR: {err}\n")
                continue
            ans = (r.get("_answer_text") or "").strip()
            if not ans:
                lines.append("\n> _(empty answer)_\n")
            else:
                lines.append("\n````")
                lines.append(ans)
                lines.append("````\n")
    lines.append("")

    SUMMARY_MD.write_text("\n".join(lines))
    print(f"wrote {SUMMARY_MD}")
    print(f"wrote {SUMMARY_CSV}")


def main() -> None:
    data = analyze()
    _write_csv(data["rows"])
    _write_md(data)
    # brief stdout summary
    ok = sum(1 for r in data["runs"] if r.get("ok"))
    print(f"analyzed {len(data['runs'])} runs ({ok} ok)")


if __name__ == "__main__":
    main()

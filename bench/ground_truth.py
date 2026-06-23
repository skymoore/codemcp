"""Compute ground_truth.json deterministically via the GitHub MCP tools.

No LLM is involved: this calls the GitHub MCP server's tools directly (through
the same `direct` MCP client the bench uses) and writes a small JSON file the
evaluator checks agent answers against. Re-run any time to refresh:

    uv run python ground_truth.py

Ground truth is over the live `skymoore` GitHub data, so if the data drifts the
truth drifts with it — the bench stays honest.
"""

from __future__ import annotations

import asyncio
import json
import re
from pathlib import Path
from typing import Any

from langchain_mcp_adapters.client import MultiServerMCPClient
from langchain_mcp_adapters.tools import load_mcp_tools

from configs import direct_mcp_config

GROUND_TRUTH_PATH = Path(__file__).resolve().parent / "ground_truth.json"
OWNER = "skymoore"


def _find_tool(tools, suffix: str):
    """Match a tool by name suffix (handles both `search_repositories` and
    `github_search_repositories` naming)."""
    for t in tools:
        if t.name == suffix or t.name.endswith("_" + suffix) or t.name.endswith(suffix):
            return t
    raise RuntimeError(f"no tool ending in {suffix!r}; have {[t.name for t in tools]}")


def _extract_text(raw: Any) -> Any:
    """If `raw` is a list of MCP content blocks, return the concatenated text;
    otherwise return `raw` unchanged. Disambiguates content blocks (type=="text")
    from already-parsed entry lists (type=="file"/"dir")."""
    if (
        isinstance(raw, list)
        and raw
        and all(isinstance(e, dict) and "type" in e for e in raw)
        and all(e.get("type") == "text" for e in raw)
    ):
        return "".join(e.get("text", "") for e in raw)
    return raw


def _parse(s: Any) -> Any:
    s = _extract_text(s)
    if isinstance(s, (dict, list)):
        return s
    if isinstance(s, str):
        txt = s.strip()
        try:
            return json.loads(txt)
        except json.JSONDecodeError:
            # github MCP sometimes wraps JSON in prose; extract the first
            # balanced JSON object/array.
            m = re.search(r"(\{.*\}|\[.*\])", txt, re.DOTALL)
            if m:
                try:
                    return json.loads(m.group(1))
                except json.JSONDecodeError:
                    pass
    return s


async def _fetch_all_repos(tools) -> list[dict]:
    sr = _find_tool(tools, "search_repositories")
    repos: list[dict] = []
    page = 1
    while True:
        raw = await sr.ainvoke(
            {"query": f"user:{OWNER}", "page": page, "perPage": 100, "minimal_output": False}
        )
        data = _parse(raw)
        items = data.get("items", []) if isinstance(data, dict) else []
        if not items:
            break
        repos.extend(items)
        total = data.get("total_count", len(repos)) if isinstance(data, dict) else len(repos)
        if len(repos) >= total or len(items) < 100:
            break
        page += 1
    return repos


async def compute() -> dict[str, Any]:
    cfg = direct_mcp_config()
    server_name = next(iter(cfg))
    client = MultiServerMCPClient(cfg)
    async with client.session(server_name) as session:
        tools = await load_mcp_tools(session)

        repos = await _fetch_all_repos(tools)
        if not repos:
            raise RuntimeError("no repos returned for skymoore")

        total_count = len(repos)

        most_starred = max(repos, key=lambda r: r.get("stargazers_count", 0))
        most_issues = max(repos, key=lambda r: r.get("open_issues_count", 0))

        ms_full = most_starred["full_name"]
        ms_owner, ms_repo = ms_full.split("/", 1)

        lc = _find_tool(tools, "list_commits")
        commits_raw = await lc.ainvoke(
            {"owner": ms_owner, "repo": ms_repo, "page": 1, "perPage": 1}
        )
        commits = _parse(commits_raw)
        commits_list = commits if isinstance(commits, list) else commits.get("items", [])
        latest_msg = ""
        if commits_list:
            msg = (
                commits_list[0].get("commit", {}).get("message", "")
                or commits_list[0].get("message", "")
                or ""
            )
            latest_msg = msg.splitlines()[0] if msg else ""

        mi_full = most_issues["full_name"]
        mi_owner, mi_repo = mi_full.split("/", 1)
        gf = _find_tool(tools, "get_file_contents")
        root_raw = await gf.ainvoke(
            {"owner": mi_owner, "repo": mi_repo, "path": ""}
        )
        root = _parse(root_raw)
        entries = root if isinstance(root, list) else root.get("entries", [])
        has_readme = any(
            (e.get("name", "") or "").upper().startswith("README")
            for e in entries
            if isinstance(e, dict)
        )

        truth = {
            "A": {"repo_count": total_count},
            "B": {
                "repo": ms_full,
                "stars": int(most_starred.get("stargazers_count", 0)),
                "latest_commit_message": latest_msg,
            },
            "C": {
                "repo": mi_full,
                "open_issues": int(most_issues.get("open_issues_count", 0)),
                "has_readme": bool(has_readme),
            },
        }
        return truth


def main() -> None:
    truth = asyncio.run(compute())
    GROUND_TRUTH_PATH.write_text(json.dumps(truth, indent=2, sort_keys=True))
    print(f"wrote {GROUND_TRUTH_PATH}")
    print(json.dumps(truth, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()

"""Core agent runner: build a LangGraph agent for one arm and run one task.

Captures, per run:
  - per-turn provider-reported token usage (input/output/cache_read/cache_creation)
  - number of model turns and tool calls
  - wall-clock time
  - the model's final answer text

Token numbers come straight from Anthropic's `usage` block (surfaced by
langchain-anthropic as `AIMessage.usage_metadata`), so they are authoritative
provider counts — not estimates. Prompt-caching components are recorded
separately so the analysis can be honest about Anthropic's tool-schema caching.
"""

from __future__ import annotations

import asyncio
import time
from typing import Any

from langchain_anthropic import ChatAnthropic
from langchain_core.messages import AIMessage, HumanMessage, SystemMessage, ToolMessage
from langchain_mcp_adapters.client import MultiServerMCPClient
from langchain_mcp_adapters.tools import load_mcp_tools

from configs import (
    BENCH_PROVIDER,
    OPENROUTER_BASE_URL,
    OPENROUTER_MODEL,
    ZEN_BASE_URL,
    ZEN_MODEL,
    active_model,
    load_api_key,
    mcp_config_for,
)
from tasks import SYSTEM_PROMPT

# create_react_agent gives the standard agent -> tools loop and accepts a plain
# string `prompt` (system prompt). (langchain v1's create_agent uses
# `system_prompt=` instead; we use the langgraph prebuilt for the stable kwarg.)
from langgraph.prebuilt import create_react_agent as _make_agent


RUN_TIMEOUT_S = 300
# LangGraph's default recursion_limit is 25 steps (~12 agent+tool turns). The
# agent is free to take as many turns as it needs, so allow generous headroom.
RECURSION_LIMIT = 80


def build_llm(api_key: str | None = None):
    """Build the chat model for the active provider (BENCH_PROVIDER).

    - `openrouter`: ChatOpenAI against OpenRouter's OpenAI-compatible endpoint
      with an automatic-prefix-caching model. This is the arm that exercises
      prompt caching, so a shape-driven tools/list_changed (which mutates the
      tool-schema prefix) can be observed busting the cache.
    - `zen` (default): ChatAnthropic against OpenCode Zen.
    """
    key = api_key or load_api_key()
    if BENCH_PROVIDER == "openrouter":
        # Same Claude family as Zen, but via OpenRouter's Anthropic-native
        # endpoint WITH prompt caching on. The tool-schema + system prefix is
        # marked for caching (see build_agent), so a shape-driven
        # tools/list_changed busts that cache — exactly what we want to measure.
        return ChatAnthropic(
            model=OPENROUTER_MODEL,
            anthropic_api_url=OPENROUTER_BASE_URL,
            api_key=key,
            temperature=0,
            max_tokens=4096,
            timeout=120,
            max_retries=2,
        )
    return ChatAnthropic(
        model=ZEN_MODEL,
        anthropic_api_url=ZEN_BASE_URL,
        api_key=key,
        temperature=0,  # maximize repeatability across repeats
        max_tokens=4096,
        timeout=120,
        max_retries=2,
    )


def _usage_of(msg: AIMessage) -> dict[str, int]:
    u = msg.usage_metadata or {}
    # langchain normalizes cache tokens differently per provider:
    #   Anthropic: flat `cache_read_input_tokens` / `cache_creation_input_tokens`
    #   OpenAI/OpenRouter: nested `input_token_details.cache_read`
    # Read both so the same code works for either provider.
    details = u.get("input_token_details") or {}
    cache_read = int(
        u.get("cache_read_input_tokens", 0)
        or details.get("cache_read", 0)
        or 0
    )
    cache_creation = int(
        u.get("cache_creation_input_tokens", 0)
        or details.get("cache_creation", 0)
        or 0
    )
    return {
        "input": int(u.get("input_tokens", 0) or 0),
        "output": int(u.get("output_tokens", 0) or 0),
        "cache_read": cache_read,
        "cache_creation": cache_creation,
    }


def _aggregate(per_turn: list[dict[str, int]]) -> dict[str, int]:
    keys = ("input", "output", "cache_read", "cache_creation")
    return {k: sum(t[k] for t in per_turn) for k in keys}


def _agent_prompt(nonce: str = ""):
    """The system prompt passed to the agent.

    For the openrouter (Anthropic-native, caching-on) provider, return a
    SystemMessage whose text block is marked `cache_control: ephemeral`. Anthropic
    caches the request prefix up to and including the marked block, and tool
    definitions sit before the system block in the request, so this caches the
    tool-schema + system prefix together. When a learned shape mutates the
    `execute_python` tool description, that prefix changes and the cache is busted
    on the next turn — exactly the effect under test. For zen, a plain string.

    `nonce` makes each run's cached prefix unique so the provider-side ephemeral
    cache (≈5-min TTL) is NOT shared across runs — otherwise an earlier run would
    warm the cache for a later one and contaminate the per-run measurement. We
    only need the WITHIN-session turn-to-turn behavior (stable prefix → cache
    hit on turn 2; mutated prefix → miss), so a per-run-unique prefix is correct.
    """
    if BENCH_PROVIDER == "openrouter":
        text = SYSTEM_PROMPT if not nonce else f"{SYSTEM_PROMPT}\n\n[run:{nonce}]"
        return SystemMessage(
            content=[
                {
                    "type": "text",
                    "text": text,
                    "cache_control": {"type": "ephemeral"},
                }
            ]
        )
    return SYSTEM_PROMPT


async def run_one(
    arm: str,
    task: dict[str, Any],
    llm: ChatAnthropic,
    *,
    label: str = "",
) -> dict[str, Any]:
    """Run a single task under one arm. Returns a run record dict."""
    cfg = mcp_config_for(arm)
    server_name = next(iter(cfg))
    client = MultiServerMCPClient(cfg)
    # Stateful session: keep the MCP connection alive for the whole agent run so
    # the upstream (github container / codemcp gateway) isn't relaunched per
    # tool call — matters for wall-time fairness and avoids docker churn.
    async with client.session(server_name) as session:
        tools = await load_mcp_tools(session)
        # Use the unique run label as the cache nonce so runs don't share the
        # provider-side ephemeral prompt cache (see _agent_prompt).
        agent = _make_agent(model=llm, tools=tools, prompt=_agent_prompt(label))

        t0 = time.perf_counter()
        error: str | None = None
        result: dict[str, Any] = {}
        try:
            result = await asyncio.wait_for(
                agent.ainvoke(
                    {"messages": [HumanMessage(content=task["prompt"])]},
                    config={"recursion_limit": RECURSION_LIMIT},
                ),
                timeout=RUN_TIMEOUT_S,
            )
        except Exception as e:  # noqa: BLE001
            error = f"{type(e).__name__}: {e}"
        wall = time.perf_counter() - t0

    msgs = result.get("messages", []) if isinstance(result, dict) else []
    per_turn: list[dict[str, int]] = []
    num_turns = 0
    tool_calls = 0
    final_answer = ""
    for m in msgs:
        if isinstance(m, AIMessage):
            num_turns += 1
            per_turn.append(_usage_of(m))
            tool_calls += len(m.tool_calls or [])
            final_answer = (
                m.content if isinstance(m.content, str) else str(m.content)
            )
    # number of tool results that came back (sanity equals tool_calls usually)
    tool_results = sum(1 for m in msgs if isinstance(m, ToolMessage))

    record = {
        "arm": arm,
        "task_id": task["id"],
        "task_name": task["name"],
        "model": active_model(),
        "endpoint": OPENROUTER_BASE_URL if BENCH_PROVIDER == "openrouter" else ZEN_BASE_URL,
        "provider": BENCH_PROVIDER,
        "label": label,
        "wall_seconds": round(wall, 3),
        "num_turns": num_turns,
        "tool_calls": tool_calls,
        "tool_results": tool_results,
        "num_tools_bound": len(tools),
        "usage_per_turn": per_turn,
        "totals": _aggregate(per_turn),
        "final_answer": final_answer,
        "ok": error is None,
        "error": error,
    }
    return record

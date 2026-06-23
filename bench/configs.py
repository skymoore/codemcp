"""Build the two MCP client configs under test and load the Zen API key.

Both arms expose the IDENTICAL upstream toolset — the GitHub MCP server
(`ghcr.io/github/github-mcp-server`, ~45 tools) — so the only variable is how
that toolset reaches the model:

  direct   -> LangGraph binds all ~45 GitHub tools directly (big per-turn schema)
  codemcp  -> LangGraph binds a single `execute_python` tool whose description
              lists 45 two-line signatures (small schema), and the agent writes
              Python that calls the GitHub tools through the codemcp gateway

The bench-local `mcp.github.json` is the SINGLE source of truth for both arms.
It uses codemcp's `{env:GITHUB_TOKEN}` interpolation; the token itself lives in
`bench/.env` (git-ignored). configs.py loads `.env` at import time so:
  - the direct arm resolves `{env:GITHUB_TOKEN}` itself, and
  - the codemcp arm inherits `GITHUB_TOKEN` in its subprocess env, letting the
    gateway interpolate it from `mcp.github.json`.

The model inference endpoint is OpenCode Zen's Anthropic-compatible endpoint:
    https://opencode.ai/zen/v1/messages
The API key is read from `~/.local/share/opencode/auth.json` (the `opencode`
provider key) — never printed or logged.
"""

from __future__ import annotations

import json
import os
import re
from pathlib import Path
from typing import Any

BENCH_DIR = Path(__file__).resolve().parent
BENCH_GITHUB_CONFIG = BENCH_DIR / "mcp.github.json"
BENCH_ENV_FILE = BENCH_DIR / ".env"

ZEN_BASE_URL = "https://opencode.ai/zen"
ZEN_MODEL = os.environ.get("BENCH_MODEL", "claude-sonnet-4-6")
OPENCODE_AUTH_PATH = Path.home() / ".local/share/opencode/auth.json"

_ENV_VAR_RE = re.compile(r"\{env:([A-Za-z_][A-Za-z0-9_]*)\}")


def _load_dotenv(path: Path = BENCH_ENV_FILE, *, override: bool = False) -> None:
    """Minimal .env loader (no python-dotenv dep). Sets os.environ entries."""
    if not path.exists():
        return
    for raw in path.read_text().splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, val = line.partition("=")
        key = key.strip()
        val = val.strip()
        # strip matching surrounding quotes
        if len(val) >= 2 and val[0] == val[-1] and val[0] in ("'", '"'):
            val = val[1:-1]
        if override or key not in os.environ:
            os.environ[key] = val


# Load secrets into the environment before anything reads them.
_load_dotenv()


def _resolve_env_vars(value: Any) -> Any:
    """Recursively replace `{env:VAR}` markers using os.environ."""
    if isinstance(value, str):
        def repl(m: re.Match) -> str:
            return os.environ.get(m.group(1), "")
        return _ENV_VAR_RE.sub(repl, value)
    if isinstance(value, dict):
        return {k: _resolve_env_vars(v) for k, v in value.items()}
    if isinstance(value, list):
        return [_resolve_env_vars(v) for v in value]
    return value


def _load_bench_mcp_json() -> dict[str, Any]:
    with open(BENCH_GITHUB_CONFIG) as f:
        return json.load(f)


def _github_entry_raw() -> dict[str, Any]:
    """The raw `github` entry from the bench-local mcp.github.json."""
    data = _load_bench_mcp_json()
    entry = data.get("mcp", {}).get("github")
    if not entry:
        raise RuntimeError(
            f"no `github` entry in {BENCH_GITHUB_CONFIG}; cannot build bench arms"
        )
    return entry


def direct_mcp_config() -> dict[str, Any]:
    """MultiServerMCPClient config: the GitHub stdio server directly.

    Resolves `{env:GITHUB_TOKEN}` here (the direct arm has no codemcp to do it).
    """
    g = _resolve_env_vars(_github_entry_raw())
    cmd = list(g.get("command", []))
    if not cmd:
        raise RuntimeError("github entry has no command")
    return {
        "github": {
            "transport": "stdio",
            "command": cmd[0],
            "args": cmd[1:],
            "env": {**os.environ, **dict(g.get("environment", {}))},
        }
    }


def codemcp_mcp_config() -> dict[str, Any]:
    """MultiServerMCPClient config: a fresh `codemcp` stdio gateway.

    The gateway is pointed at the bench-local mcp.github.json (only `github`
    enabled) so its exposed `execute_python` SDK covers exactly the same
    GitHub tools the direct arm binds. `GITHUB_TOKEN` is inherited from
    os.environ (loaded from .env) so codemcp can interpolate `{env:GITHUB_TOKEN}`
    in mcp.github.json itself.
    """
    codemcp_bin = os.environ.get("CODEMCP_BIN", _resolve_codemcp_bin())
    env = {
        **os.environ,
        "CODEMCP_CONFIG": str(BENCH_GITHUB_CONFIG),
        # Keep the bench isolated from any shared HTTP gateway on :3388.
        "CODEMCP_TRANSPORT": "stdio",
        "CODEMCP_LOG": "warn",
    }
    # Guard: refuse to start if the token isn't visible to the gateway.
    if not env.get("GITHUB_TOKEN"):
        raise RuntimeError(
            f"GITHUB_TOKEN missing — populate {BENCH_ENV_FILE} (git-ignored)"
        )
    return {
        "codemcp": {
            "transport": "stdio",
            "command": codemcp_bin,
            "args": [],
            "env": env,
        }
    }


def _resolve_codemcp_bin() -> str:
    from shutil import which

    p = which("codemcp")
    if not p:
        raise RuntimeError(
            "`codemcp` not found on PATH; set CODEMCP_BIN or install it"
        )
    return p


def load_zen_api_key() -> str:
    """Read the OpenCode Zen API key from opencode's auth.json (`opencode` key)."""
    with open(OPENCODE_AUTH_PATH) as f:
        auth = json.load(f)
    entry = auth.get("opencode", {})
    key = entry.get("key")
    if not key:
        raise RuntimeError(
            f"no `opencode.key` in {OPENCODE_AUTH_PATH}; sign in at "
            "https://opencode.ai/auth and run opencode /connect for Zen"
        )
    return key


ARMS = ("direct", "codemcp")


def mcp_config_for(arm: str) -> dict[str, Any]:
    if arm == "direct":
        return direct_mcp_config()
    if arm == "codemcp":
        return codemcp_mcp_config()
    raise ValueError(f"unknown arm: {arm!r}")

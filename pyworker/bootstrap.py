#!/usr/bin/env python3
"""codemcp Python worker.

Self-provisions the `websockets` package if missing, connects to the gateway's
WebSocket control channel, authenticates with a shared token, then serves `run`
requests by executing user code. User code calls the generated SDK functions,
each of which dispatches a `call_tool` request back to the gateway over the same
WebSocket.

Environment (set by the gateway):
  CODEMCP_CONTROL_URL        ws://host:port  (required)
  CODEMCP_CONTROL_TOKEN      shared secret    (required)
  CODEMCP_SDK_DIR            dir containing generated sdk.py (added to sys.path)
  CODEMCP_WS_AUTO_INSTALL    "true"/"false"   (default true)
  CODEMCP_WS_VERSION         pin version
  CODEMCP_WS_PIP_ARGS        extra pip args (space separated)
  CODEMCP_WS_CACHE_DIR       dir for the self-installed websockets package
"""

import asyncio
import contextlib
import io
import json
import os
import subprocess
import sys
import threading
import traceback


def _ensure_websockets():
    """Import `websockets`, installing it into a private dir if necessary."""
    try:
        import websockets  # noqa: F401
        return
    except ImportError:
        pass

    if os.environ.get("CODEMCP_WS_AUTO_INSTALL", "true").lower() not in ("1", "true", "yes", "on"):
        sys.stderr.write("codemcp worker: websockets missing and auto-install disabled\n")
        sys.exit(3)

    cache_dir = os.environ.get("CODEMCP_WS_CACHE_DIR") or os.path.join(
        os.path.expanduser("~"), ".cache", "codemcp", "pylib"
    )
    os.makedirs(cache_dir, exist_ok=True)

    if cache_dir not in sys.path:
        sys.path.insert(0, cache_dir)
    try:
        import websockets  # noqa: F401
        return
    except ImportError:
        pass

    pkg = "websockets"
    version = os.environ.get("CODEMCP_WS_VERSION")
    if version:
        pkg = f"websockets=={version}"
    cmd = [sys.executable, "-m", "pip", "install", "--target", cache_dir, pkg]
    extra = os.environ.get("CODEMCP_WS_PIP_ARGS", "").split()
    cmd.extend(extra)

    sys.stderr.write(f"codemcp worker: installing {pkg} into {cache_dir}\n")
    result = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if result.returncode != 0:
        sys.stderr.write(result.stdout.decode("utf-8", "replace"))
        sys.stderr.write("codemcp worker: failed to install websockets\n")
        sys.exit(3)

    import websockets  # noqa: F401


_ensure_websockets()

import websockets  # noqa: E402


class Dispatcher:
    """Bridges synchronous user-code SDK calls to the async WebSocket.

    User code runs in a worker thread; SDK calls schedule a `call_tool` request
    on the event loop and block the thread until the response arrives.
    """

    def __init__(self, ws, loop):
        self._ws = ws
        self._loop = loop
        self._pending = {}
        self._counter = 0
        self._lock = threading.Lock()

    def _next_id(self):
        with self._lock:
            self._counter += 1
            return f"ct-{self._counter}"

    async def handle_response(self, msg):
        rid = msg.get("id")
        fut = self._pending.pop(rid, None)
        if fut is not None and not fut.done():
            fut.set_result(msg)

    def call_tool(self, server, tool, args):
        """Synchronous: send call_tool and block for the result."""
        rid = self._next_id()
        request = {
            "jsonrpc": "2.0",
            "id": rid,
            "method": "call_tool",
            "params": {"server": server, "tool": tool, "args": args},
        }

        fut = self._loop.create_future()
        self._pending[rid] = fut

        async def _send():
            await self._ws.send(json.dumps(request))

        asyncio.run_coroutine_threadsafe(_send(), self._loop)

        # Block this (worker) thread until the loop resolves the future.
        result_msg = asyncio.run_coroutine_threadsafe(
            _await_future(fut), self._loop
        ).result()

        if "error" in result_msg and result_msg["error"] is not None:
            raise RuntimeError(f"tool {server}/{tool} failed: {result_msg['error']}")
        return _unwrap_tool_result(result_msg.get("result"))


async def _await_future(fut):
    return await fut


def _unwrap_tool_result(result):
    """Turn an MCP CallToolResult into something ergonomic for Python.

    Prefer structuredContent; else join text content; else return raw.
    """
    if not isinstance(result, dict):
        return result
    if result.get("structuredContent") is not None:
        return result["structuredContent"]
    content = result.get("content")
    if isinstance(content, list):
        texts = []
        for item in content:
            if isinstance(item, dict) and item.get("type") == "text":
                texts.append(item.get("text", ""))
        if texts:
            joined = "\n".join(texts)
            # Try to parse JSON text payloads for convenience.
            try:
                return json.loads(joined)
            except (ValueError, TypeError):
                return joined
    return result


def _exec_user_code(code, sdk_module, dispatcher):
    """Execute user code, returning (result, stdout, stderr, error)."""
    namespace = {"__name__": "__codemcp__"}
    # Inject all SDK functions.
    for name in dir(sdk_module):
        if not name.startswith("_"):
            namespace[name] = getattr(sdk_module, name)
    # Wire dispatch into the SDK module.
    sdk_module._codemcp_dispatch = dispatcher.call_tool

    out, err = io.StringIO(), io.StringIO()
    result_value = None
    error = None
    try:
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            compiled = compile(code, "<codemcp>", "exec")
            exec(compiled, namespace)
            # Convention: `result` variable, else None.
            result_value = namespace.get("result")
    except Exception:
        error = traceback.format_exc()

    return result_value, out.getvalue(), err.getvalue(), error


def _json_safe(value):
    try:
        json.dumps(value)
        return value
    except (TypeError, ValueError):
        return repr(value)


async def main():
    url = os.environ["CODEMCP_CONTROL_URL"]
    token = os.environ["CODEMCP_CONTROL_TOKEN"]
    sdk_dir = os.environ.get("CODEMCP_SDK_DIR", ".")

    if sdk_dir not in sys.path:
        sys.path.insert(0, sdk_dir)
    import sdk as sdk_module  # generated

    loop = asyncio.get_running_loop()

    async with websockets.connect(url, max_size=None) as ws:
        # First frame: auth token.
        await ws.send(token)

        dispatcher = Dispatcher(ws, loop)

        async for raw in ws:
            try:
                msg = json.loads(raw)
            except ValueError:
                continue

            method = msg.get("method")
            if method == "run":
                # Run user code as a background task so the read loop keeps
                # servicing the SDK's call_tool round-trips while it executes.
                asyncio.create_task(_handle_run(ws, msg, sdk_module, dispatcher))
            elif method is None:
                # A response to one of our call_tool requests.
                await dispatcher.handle_response(msg)


async def _handle_run(ws, msg, sdk_module, dispatcher):
    code = msg.get("params", {}).get("code", "")
    rid = msg.get("id")
    # Run user code off the event loop so SDK calls can round-trip.
    result_value, stdout, stderr, error = await asyncio.to_thread(
        _exec_user_code, code, sdk_module, dispatcher
    )
    response = {
        "jsonrpc": "2.0",
        "id": rid,
        "result": {
            "result": _json_safe(result_value),
            "stdout": stdout,
            "stderr": stderr,
            "error": error,
        },
    }
    await ws.send(json.dumps(response))


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass

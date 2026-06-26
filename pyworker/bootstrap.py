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

import ast
import asyncio
import builtins
import contextlib
import difflib
import inspect
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


# ── concurrent execution context ─────────────────────────────
#
# User code runs in a worker thread; the WebSocket lives on the asyncio event
# loop in the main thread. An SDK call *fires* its `call_tool` request onto the
# loop immediately (so the request is on the wire right away) and returns a
# `Pending` handle that resolves lazily — it blocks only when user code first
# reads its value (attribute, index, iteration, etc.).
#
# This gives concurrency for free: any calls issued before a result is read are
# already in flight by the time the first value is accessed, so their round-trips
# overlap. No special syntax is required.

# Per-worker-thread handle to the event loop, so a `Pending` can reach it to
# resolve even though it runs on the worker thread.
_thread_local = threading.local()


class ToolError(RuntimeError):
    """Raised when an upstream tool call fails."""


class Pending:
    """A handle to an in-flight `call_tool` request.

    The request is already on the wire. The result is fetched lazily and cached
    on first access. Most value-like operations (indexing, attribute access,
    iteration, truthiness, str/repr, equality) transparently resolve, so in the
    common case user code can treat a `Pending` exactly like the returned value.

    Call ``.result()`` to force resolution explicitly.
    """

    __slots__ = ("_fut", "_loop", "_server", "_tool", "_resolved", "_value")

    def __init__(self, fut, loop, server, tool):
        self._fut = fut
        self._loop = loop
        self._server = server
        self._tool = tool
        self._resolved = False
        self._value = None

    def result(self, timeout=None):
        """Block until the round-trip completes and return the unwrapped value."""
        if self._resolved:
            return self._value
        msg = asyncio.run_coroutine_threadsafe(
            _await_future(self._fut), self._loop
        ).result(timeout)
        if isinstance(msg, dict) and msg.get("error") is not None:
            raise ToolError(f"tool {self._server}/{self._tool} failed: {msg['error']}")
        value = _unwrap_tool_result(msg.get("result") if isinstance(msg, dict) else msg)
        self._value = value
        self._resolved = True
        return value

    # ── transparent resolution for ergonomic sequential use ──
    def __getitem__(self, key):
        return self.result()[key]

    def __getattr__(self, name):
        # Internal slot names are served by the descriptor protocol and never
        # reach here once set. Guard against recursion if accessed before init
        # completes (e.g. during unpickling) and shield dunder lookups so the
        # proxy doesn't accidentally claim to implement arbitrary protocols.
        if name.startswith("__") and name.endswith("__"):
            raise AttributeError(name)
        return getattr(self.result(), name)

    def __iter__(self):
        return iter(self.result())

    def __len__(self):
        return len(self.result())

    def __contains__(self, item):
        return item in self.result()

    def __bool__(self):
        return bool(self.result())

    def __eq__(self, other):
        return self.result() == other

    def __ne__(self, other):
        return self.result() != other

    def __hash__(self):
        return hash(self.result())

    def __repr__(self):
        if self._resolved:
            return repr(self._value)
        return f"<Pending {self._server}/{self._tool}>"

    def __str__(self):
        return str(self.result())


class Dispatcher:
    """Bridges synchronous user-code SDK calls to the async WebSocket.

    An SDK call schedules a `call_tool` request on the event loop (firing it
    immediately) and returns a `Pending` handle. The handle blocks only when its
    value is actually needed, which is what lets independent calls overlap.
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
        """Fire a `call_tool` request and return a lazily-resolved `Pending`."""
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

        # Fire now: the request is on the wire before we return.
        asyncio.run_coroutine_threadsafe(_send(), self._loop)
        return Pending(fut, self._loop, server, tool)


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


# ── pre-flight static validation ─────────────────────────────
#
# Before executing agent-written code we statically check it against the SDK
# contract: catch syntax errors, calls to a misspelled SDK function, and bad
# arguments to a known SDK function (unknown kwarg, missing required arg). This
# turns a wasted execution round-trip (run broken code → return a raw traceback →
# model retries on a more expensive turn) into a precise, structured hint the
# model can act on in the same turn.
#
# The check is deliberately conservative: it only flags things it is confident
# about. Locally-defined functions, builtins, attribute calls (`x.foo()`), and
# dynamic patterns are never flagged. The real `sdk_module` is the source of
# truth for signatures (via `inspect.signature`), so the contract can never drift
# from the generated SDK.

# Names always available in the exec namespace besides the SDK functions.
_RUNTIME_INJECTED_NAMES = frozenset({"Pending", "ToolError", "result"})

# How close a bare-name call must be to an SDK function name before we treat it
# as a typo worth flagging (difflib ratio, 0..1). High to avoid false positives.
_SUGGEST_CUTOFF = 0.7


def _sdk_function_names(sdk_module):
    """Public functions exposed by the generated SDK module, keyed by name."""
    names = {}
    for name in dir(sdk_module):
        if name.startswith("_"):
            continue
        obj = getattr(sdk_module, name, None)
        if inspect.isfunction(obj):
            names[name] = obj
    return names


def _collect_bound_names(tree):
    """Names the user code itself defines (so we don't flag them as unknown).

    Covers assignments, function/class defs, imports, comprehension targets,
    `for` targets, `with ... as`, `except ... as`, and function parameters.
    Conservative: any name that *might* be locally bound is collected.
    """
    bound = set()
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            bound.add(node.name)
            args = getattr(node, "args", None)
            if args is not None:
                for a in (
                    list(args.posonlyargs)
                    + list(args.args)
                    + list(args.kwonlyargs)
                ):
                    bound.add(a.arg)
                if args.vararg:
                    bound.add(args.vararg.arg)
                if args.kwarg:
                    bound.add(args.kwarg.arg)
        elif isinstance(node, ast.Lambda):
            args = node.args
            for a in (
                list(args.posonlyargs) + list(args.args) + list(args.kwonlyargs)
            ):
                bound.add(a.arg)
            if args.vararg:
                bound.add(args.vararg.arg)
            if args.kwarg:
                bound.add(args.kwarg.arg)
        elif isinstance(node, (ast.Import, ast.ImportFrom)):
            for alias in node.names:
                bound.add((alias.asname or alias.name).split(".")[0])
        elif isinstance(node, ast.Name) and isinstance(node.ctx, ast.Store):
            bound.add(node.id)
    return bound


def _format_call_error(fn_name, problem, suggestion=None):
    msg = f"{fn_name}: {problem}"
    if suggestion:
        msg += f" {suggestion}"
    return msg


def _validate_call(node, fn_name, fn, errors):
    """Check one call to a known SDK function against its real signature."""
    try:
        sig = inspect.signature(fn)
    except (TypeError, ValueError):
        return

    params = sig.parameters
    valid_kw = [
        name
        for name, p in params.items()
        if p.kind
        in (inspect.Parameter.POSITIONAL_OR_KEYWORD, inspect.Parameter.KEYWORD_ONLY)
    ]
    accepts_var_kw = any(
        p.kind == inspect.Parameter.VAR_KEYWORD for p in params.values()
    )
    accepts_var_pos = any(
        p.kind == inspect.Parameter.VAR_POSITIONAL for p in params.values()
    )
    required = [
        name
        for name, p in params.items()
        if p.default is inspect.Parameter.empty
        and p.kind
        in (
            inspect.Parameter.POSITIONAL_ONLY,
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
            inspect.Parameter.KEYWORD_ONLY,
        )
    ]

    where = f"(line {node.lineno})"

    # Unknown keyword arguments (skip **kwargs spreads, which have arg=None).
    provided_kw = set()
    for kw in node.keywords:
        if kw.arg is None:
            # **something spread — can't statically resolve; give up on this call.
            return
        provided_kw.add(kw.arg)
        if kw.arg not in valid_kw and not accepts_var_kw:
            near = difflib.get_close_matches(kw.arg, valid_kw, n=1, cutoff=_SUGGEST_CUTOFF)
            hint = f"Did you mean `{near[0]}`?" if near else (
                f"Valid arguments: {', '.join(valid_kw) or '(none)'}."
            )
            errors.append(
                _format_call_error(
                    fn_name,
                    f"unknown argument `{kw.arg}` {where}.",
                    hint,
                )
            )

    # Positional args: only meaningful if the function takes no *args. If the
    # caller passes more positionals than there are parameters, flag it.
    n_positional = len(node.args)
    has_starred = any(isinstance(a, ast.Starred) for a in node.args)
    if not accepts_var_pos and not has_starred and n_positional > len(valid_kw):
        errors.append(
            _format_call_error(
                fn_name,
                f"takes {len(valid_kw)} argument(s) but {n_positional} "
                f"positional were given {where}.",
            )
        )

    # Missing required arguments. Account for positionally-supplied params.
    if not has_starred:
        positionally_filled = set(valid_kw[:n_positional])
        missing = [
            name
            for name in required
            if name not in provided_kw and name not in positionally_filled
        ]
        if missing:
            errors.append(
                _format_call_error(
                    fn_name,
                    f"missing required argument(s) {', '.join('`' + m + '`' for m in missing)} {where}.",
                )
            )


def _validate_user_code(code, sdk_module, keysets=None):
    """Statically validate `code` against the SDK contract.

    Returns ``None`` if the code passes the pre-flight checks, otherwise a
    structured, human-readable error string describing each problem found.

    `keysets` is the optional `fn_name -> KeySet` validation map shipped by the
    gateway; when present, literal field access on values returned by SDK calls
    is strictly checked against the learned/declared key structure.
    """
    # 1. Syntax.
    try:
        tree = ast.parse(code, filename="<codemcp>", mode="exec")
    except SyntaxError as e:
        loc = f"line {e.lineno}" + (f", col {e.offset}" if e.offset else "")
        detail = (e.text or "").strip()
        msg = f"SyntaxError: {e.msg} ({loc})."
        if detail:
            msg += f"\n    {detail}"
        return msg

    sdk_fns = _sdk_function_names(sdk_module)
    if not sdk_fns:
        # No SDK contract to check against; only syntax mattered.
        return None

    sdk_names = list(sdk_fns)
    bound = _collect_bound_names(tree)
    builtin_names = set(dir(builtins))

    errors = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call) or not isinstance(node.func, ast.Name):
            continue
        name = node.func.id

        if name in sdk_fns:
            _validate_call(node, name, sdk_fns[name], errors)
            continue

        # A bare call to an unknown name. Only flag it as a typo when it is a
        # close match to an SDK function (high-confidence). Anything that could
        # be a local, builtin, or import is left alone.
        if name in bound or name in builtin_names or name in _RUNTIME_INJECTED_NAMES:
            continue
        near = difflib.get_close_matches(name, sdk_names, n=1, cutoff=_SUGGEST_CUTOFF)
        if near:
            errors.append(
                _format_call_error(
                    name,
                    f"is not a known SDK function (line {node.lineno}).",
                    f"Did you mean `{near[0]}`?",
                )
            )

    # 2. Strict return-field validation (only when the gateway shipped keysets).
    if keysets:
        _validate_field_access(tree, sdk_fns, keysets, errors)

    if not errors:
        return None

    header = (
        "Pre-flight validation failed (code was not executed). "
        "Fix these and resend:\n"
    )
    return header + "\n".join(f"  - {e}" for e in errors)


# ── return-field validation ──────────────────────────────────
#
# Track variables assigned directly from an SDK call (`x = github_get_me(...)`)
# and strictly check literal field access on them (`x["lgoin"]`, `x.lgoin`,
# nested `x["user"]["lgoin"]`) against the full key structure the gateway
# learned/declared for that tool. Because the keyset is COMPLETE (no field caps,
# unioned across observed variants), "key not present" is a trustworthy signal,
# so we hard-flag it with a difflib suggestion — same UX as the kwarg check.
#
# Conservative by construction: only simple `name = sdk_fn(...)` bindings are
# tracked; only literal subscripts/attributes are descended; the moment a chain
# hits a non-object node, an unknown key, dynamic access, or array indexing we
# stop. A tracked name that is reassigned or whose binding we can't prove is
# dropped from tracking.


def _keyset_kind(ks):
    return ks.get("k") if isinstance(ks, dict) else None


def _keyset_descend(ks, key):
    """Return the child KeySet for `key`, or None if `key` is not a valid field.

    Returns the sentinel ("__leaf__", None) semantics via two channels:
      - (True, child)  -> `key` is valid; `child` is its KeySet (may be leaf)
      - (False, keys)  -> `key` is NOT valid; `keys` is the list of valid keys
      - (None, None)   -> cannot decide here (not an object / opaque); skip
    """
    kind = _keyset_kind(ks)
    if kind != "object":
        # Arrays and leaves can't be validated by a literal string key.
        return (None, None)
    fields = ks.get("fields") or {}
    if not isinstance(fields, dict) or not fields:
        return (None, None)
    if key in fields:
        return (True, fields[key])
    return (False, list(fields.keys()))


def _literal_access_chain(node):
    """Peel a chain of literal subscripts/attributes off the OUTERMOST expr.

    Given an AST node that is the *target* of access (e.g. the whole
    `x["user"]["login"]`), return `(root_name, [keys...])` where root_name is the
    base `ast.Name` id and keys is the ordered list of literal string keys.
    Returns (None, None) if the chain isn't a pure literal access on a Name.
    """
    keys = []
    cur = node
    while True:
        if isinstance(cur, ast.Subscript):
            sl = cur.slice
            # Py3.9+: slice is the expression directly.
            if isinstance(sl, ast.Constant) and isinstance(sl.value, str):
                keys.append(sl.value)
                cur = cur.value
                continue
            return (None, None)  # non-literal / numeric / slice -> bail
        if isinstance(cur, ast.Attribute):
            keys.append(cur.attr)
            cur = cur.value
            continue
        if isinstance(cur, ast.Name):
            keys.reverse()
            return (cur.id, keys)
        return (None, None)


def _validate_field_access(tree, sdk_fns, keysets, errors):
    # var -> fn_name, for `name = sdk_fn(...)` single-target assignments.
    # If a name is EVER assigned to a non-SDK value (anywhere in the snippet), we
    # refuse to track it: we can't prove which binding a given access refers to
    # without flow analysis, so we stay silent rather than risk a false positive.
    sdk_bindings = {}  # var -> fn_name (last SDK-call assignment)
    non_sdk_assigned = set()

    for node in ast.walk(tree):
        if not isinstance(node, ast.Assign):
            continue
        if len(node.targets) != 1 or not isinstance(node.targets[0], ast.Name):
            continue
        var = node.targets[0].id
        val = node.value
        if (
            isinstance(val, ast.Call)
            and isinstance(val.func, ast.Name)
            and val.func.id in sdk_fns
            and val.func.id in keysets
        ):
            sdk_bindings[var] = val.func.id
        else:
            non_sdk_assigned.add(var)

    tracked = {
        var: fn for var, fn in sdk_bindings.items() if var not in non_sdk_assigned
    }
    if not tracked:
        return

    # Nodes that are the `.value` of an enclosing access are inner links of a
    # larger chain; only validate the OUTERMOST node of each chain so a nested
    # access like x["user"]["login"] is checked once, not once per link.
    inner = set()
    for n in ast.walk(tree):
        if isinstance(n, (ast.Subscript, ast.Attribute)):
            inner.add(id(n.value))

    for node in ast.walk(tree):
        if not isinstance(node, (ast.Subscript, ast.Attribute)):
            continue
        if id(node) in inner:
            continue  # not the outermost link of its chain
        root, keys = _literal_access_chain(node)
        if root is None or root not in tracked or not keys:
            continue

        fn_name = tracked[root]
        ks = keysets.get(fn_name)
        lineno = getattr(node, "lineno", 0)
        # Descend key by key; stop at first undecidable or invalid step.
        for depth, key in enumerate(keys):
            ok, info = _keyset_descend(ks, key)
            if ok is None:
                break  # can't validate here (array/leaf/opaque) -> stop
            if ok is False:
                valid = info
                where = f"(line {lineno})"
                near = difflib.get_close_matches(key, valid, n=1, cutoff=_SUGGEST_CUTOFF)
                path = "".join(f"[{k!r}]" for k in keys[:depth]) or "result"
                hint = (
                    f"Did you mean `{near[0]}`?"
                    if near
                    else f"Available fields: {', '.join(valid)}."
                )
                errors.append(
                    _format_call_error(
                        fn_name,
                        f"result{('' if path == 'result' else path)} has no field "
                        f"`{key}` {where}.",
                        hint,
                    )
                )
                break
            ks = info  # descend


def _exec_user_code(code, sdk_module, dispatcher, keysets=None):
    """Execute user code, returning (result, stdout, stderr, error)."""
    # Pre-flight: reject statically-broken code before running anything, so the
    # model gets a precise hint instead of a post-hoc traceback.
    validation_error = _validate_user_code(code, sdk_module, keysets)
    if validation_error is not None:
        return None, "", "", validation_error

    namespace = {"__name__": "__codemcp__"}
    # Inject SDK functions directly — each returns a Pending when called.
    for name in dir(sdk_module):
        if not name.startswith("_"):
            namespace[name] = getattr(sdk_module, name)

    # Wire dispatch into the SDK module.
    sdk_module._codemcp_dispatch = dispatcher.call_tool

    namespace["Pending"] = Pending
    namespace["ToolError"] = ToolError
    # Remember the loop so Pending can reach it from this thread.
    _thread_local._loop = dispatcher._loop

    out, err = io.StringIO(), io.StringIO()
    result_value = None
    error = None
    try:
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            compiled = compile(code, "<codemcp>", "exec")
            exec(compiled, namespace)
            # Convention: `result` variable, else None.
            result_value = namespace.get("result")
            # If the result is still a Pending, resolve it before returning.
            if isinstance(result_value, Pending):
                result_value = result_value.result()
    except Exception:
        error = traceback.format_exc()

    return result_value, out.getvalue(), err.getvalue(), error


def _json_safe(value):
    if isinstance(value, Pending):
        value = value.result()
    try:
        json.dumps(value)
        return value
    except (TypeError, ValueError):
        return repr(value)


class SdkHolder:
    """Mutable container for the current SDK module so `reload` can swap it."""

    def __init__(self, module, sdk_dir):
        self.module = module
        self.sdk_dir = sdk_dir
        # fn_name -> KeySet dict (the serialized form shipped by the gateway),
        # used for strict pre-flight return-field validation. Empty when the
        # shape-learning feature is off.
        self.keysets = {}

    def reload(self, source):
        """Overwrite sdk.py with `source` and re-import the module.

        The write is best-effort: on a read-only filesystem (the Docker backend
        bind-mounts the workdir read-only), the gateway has already updated the
        mounted file, so we skip the write and just re-import it. On a writable
        filesystem (HOST backend) the write is what materializes the new SDK.
        """
        import importlib

        path = os.path.join(self.sdk_dir, "sdk.py")
        try:
            with open(path, "w") as f:
                f.write(source)
        except OSError:
            # Read-only mount: the gateway owns the file and has already
            # written the new source there. Re-import picks up that content.
            pass
        # Drop any cached bytecode so the new source is used.
        importlib.invalidate_caches()
        self.module = importlib.reload(self.module)


async def main():
    url = os.environ["CODEMCP_CONTROL_URL"]
    token = os.environ["CODEMCP_CONTROL_TOKEN"]
    sdk_dir = os.environ.get("CODEMCP_SDK_DIR", ".")

    if sdk_dir not in sys.path:
        sys.path.insert(0, sdk_dir)
    import sdk as sdk_module  # generated

    holder = SdkHolder(sdk_module, sdk_dir)
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
                asyncio.create_task(_handle_run(ws, msg, holder, dispatcher))
            elif method == "reload":
                await _handle_reload(ws, msg, holder)
            elif method == "set_shapes":
                await _handle_set_shapes(ws, msg, holder)
            elif method is None:
                # A response to one of our call_tool requests.
                await dispatcher.handle_response(msg)


async def _handle_reload(ws, msg, holder):
    rid = msg.get("id")
    source = msg.get("params", {}).get("sdk", "")
    error = None
    try:
        await asyncio.to_thread(holder.reload, source)
    except Exception:
        error = traceback.format_exc()
    response = {
        "jsonrpc": "2.0",
        "id": rid,
        "result": {"result": None, "stdout": "", "stderr": "", "error": error},
    }
    await ws.send(json.dumps(response))


async def _handle_set_shapes(ws, msg, holder):
    """Store the gateway's `fn_name -> KeySet` validation map on the holder.

    The map is used by the pre-flight validator to strictly check literal field
    access on values returned by SDK calls. Replacing wholesale each push keeps
    the worker's view in lockstep with the gateway's accumulated knowledge.
    """
    rid = msg.get("id")
    error = None
    try:
        shapes = msg.get("params", {}).get("shapes", {})
        holder.keysets = shapes if isinstance(shapes, dict) else {}
    except Exception:
        error = traceback.format_exc()
    response = {
        "jsonrpc": "2.0",
        "id": rid,
        "result": {"result": None, "stdout": "", "stderr": "", "error": error},
    }
    await ws.send(json.dumps(response))


async def _handle_run(ws, msg, holder, dispatcher):
    code = msg.get("params", {}).get("code", "")
    rid = msg.get("id")
    # Run user code off the event loop so SDK calls can round-trip.
    result_value, stdout, stderr, error = await asyncio.to_thread(
        _exec_user_code, code, holder.module, dispatcher, holder.keysets
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
    except Exception as exc:  # pragma: no cover
        # The gateway closing the control channel is a normal shutdown signal;
        # exit quietly rather than dumping a traceback.
        import websockets.exceptions as _wse

        if isinstance(exc, (_wse.ConnectionClosed, ConnectionError, OSError)):
            sys.exit(0)
        raise

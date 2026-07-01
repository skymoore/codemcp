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
import time
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


def _classify_error(message):
    """Best-effort normalization of an upstream error string into a kind."""
    m = (message or "").lower()
    if "timed out" in m or "timeout" in m or "deadline" in m:
        return "timeout"
    if "unauthor" in m or "forbidden" in m or "401" in m or "403" in m or "auth" in m:
        return "auth"
    if "connection" in m or "transport" in m or "closed" in m or "refused" in m:
        return "transport"
    if "not found" in m or "404" in m:
        return "not_found"
    return "upstream_error"


class ToolError(RuntimeError):
    """Raised when an upstream tool call fails.

    Carries structured context so both user code and the run-result formatter can
    localize the failure precisely (which server/tool, which args, how long it
    took, and a normalized ``kind``) instead of parsing a raw string.
    """

    def __init__(self, server, tool, message, args=None, elapsed_ms=None, kind=None):
        self.server = server
        self.tool = tool
        self.tool_message = message
        # Note: do NOT name this ``self.args`` — RuntimeError uses ``.args`` for
        # its own positional init tuple. Use ``call_args`` for the tool arguments.
        self.call_args = args or {}
        self.elapsed_ms = elapsed_ms
        self.kind = kind or _classify_error(message)
        super().__init__(
            f"tool {server}/{tool} failed [{self.kind}]: {message}"
        )

    def as_dict(self, redact=True):
        return {
            "kind": self.kind,
            "server": self.server,
            "tool": self.tool,
            "args": _redact_args(self.call_args) if redact else self.call_args,
            "message": self.tool_message,
            "elapsed_ms": self.elapsed_ms,
        }


# Argument keys whose values are sensitive and should never appear in traces /
# audit output. Matched case-insensitively as substrings.
_SECRET_ARG_HINTS = ("token", "secret", "password", "passwd", "apikey", "api_key",
                     "authorization", "auth", "credential", "private", "cookie")


def _redact_args(args):
    """Return a shallow copy of ``args`` with sensitive values masked."""
    if not isinstance(args, dict):
        return args
    out = {}
    for k, v in args.items():
        kl = str(k).lower()
        if any(h in kl for h in _SECRET_ARG_HINTS):
            out[k] = "***"
        elif isinstance(v, str) and len(v) > 256:
            out[k] = v[:253] + "..."
        else:
            out[k] = v
    return out


class Pending:
    """A handle to an in-flight `call_tool` request.

    The request is already on the wire. The result is fetched lazily and cached
    on first access. Most value-like operations (indexing, attribute access,
    iteration, truthiness, str/repr, equality) transparently resolve, so in the
    common case user code can treat a `Pending` exactly like the returned value.

    Call ``.result()`` to force resolution explicitly. Pass ``timeout`` (seconds)
    to bound a single call, or use the per-call ``timeout_ms`` SDK kwarg.
    """

    __slots__ = (
        "_fut", "_loop", "_server", "_tool", "_args", "_resolved", "_value",
        "_error", "_trace", "_started", "_timeout", "_mutation",
    )

    def __init__(self, fut, loop, server, tool, args=None, trace=None,
                 timeout=None, mutation=False):
        self._fut = fut
        self._loop = loop
        self._server = server
        self._tool = tool
        self._args = args or {}
        self._resolved = False
        self._value = None
        self._error = None
        self._trace = trace
        self._started = time.monotonic()
        self._timeout = timeout
        self._mutation = mutation

    def _record(self, ok, kind=None):
        if self._trace is not None:
            self._trace.record(
                server=self._server,
                tool=self._tool,
                args=self._args,
                ok=ok,
                kind=kind,
                elapsed_ms=int((time.monotonic() - self._started) * 1000),
                mutation=self._mutation,
            )

    def result(self, timeout=None):
        """Block until the round-trip completes and return the unwrapped value.

        Raises :class:`ToolError` (structured) if the upstream call failed or the
        per-call timeout elapsed.
        """
        if self._resolved:
            return self._value
        if self._error is not None:
            raise self._error
        eff_timeout = timeout if timeout is not None else self._timeout
        try:
            msg = asyncio.run_coroutine_threadsafe(
                _await_future(self._fut), self._loop
            ).result(eff_timeout)
        except (TimeoutError, asyncio.TimeoutError):
            elapsed = int((time.monotonic() - self._started) * 1000)
            self._error = ToolError(
                self._server, self._tool,
                f"call exceeded timeout of {eff_timeout}s",
                args=self._args, elapsed_ms=elapsed, kind="timeout",
            )
            self._record(ok=False, kind="timeout")
            raise self._error
        if isinstance(msg, dict) and msg.get("error") is not None:
            elapsed = int((time.monotonic() - self._started) * 1000)
            self._error = ToolError(
                self._server, self._tool, str(msg["error"]),
                args=self._args, elapsed_ms=elapsed,
            )
            self._record(ok=False, kind=self._error.kind)
            raise self._error
        value = _unwrap_tool_result(msg.get("result") if isinstance(msg, dict) else msg)
        self._value = value
        self._resolved = True
        self._record(ok=True)
        return value

    def settled(self, timeout=None):
        """Resolve without raising. Returns ``{"ok": True, "value": ...}`` or
        ``{"ok": False, "error": {...}}`` — the allSettled shape for one call."""
        try:
            return {"ok": True, "value": self.result(timeout)}
        except ToolError as e:
            return {"ok": False, "error": e.as_dict()}
        except Exception as e:  # pragma: no cover - defensive
            return {"ok": False, "error": {"kind": "worker_error", "message": str(e)}}

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
        return f"<unresolved Pending {self._server}/{self._tool} (reading it will block)>"

    def __str__(self):
        return str(self.result())


class _TraceSink:
    """Thread-safe collector of per-call trace + mutation-audit entries."""

    def __init__(self):
        self._lock = threading.Lock()
        self._entries = []

    def record(self, server, tool, args, ok, kind, elapsed_ms, mutation):
        entry = {
            "server": server,
            "tool": tool,
            "ok": ok,
            "elapsed_ms": elapsed_ms,
            "mutation": mutation,
        }
        if kind:
            entry["kind"] = kind
        with self._lock:
            self._entries.append(entry)

    def trace(self):
        with self._lock:
            return list(self._entries)

    def mutations(self):
        with self._lock:
            return [
                {"server": e["server"], "tool": e["tool"], "ok": e["ok"]}
                for e in self._entries
                if e.get("mutation")
            ]


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
        # Per-run context, set by _exec_user_code before running user code.
        self._trace = None
        self._dry_run = False
        self._mutations = frozenset()

    def begin_run(self, trace, dry_run, mutations):
        """Configure per-run behaviour (trace sink, dry-run, mutation set)."""
        self._trace = trace
        self._dry_run = bool(dry_run)
        self._mutations = frozenset(mutations or ())

    def _next_id(self):
        with self._lock:
            self._counter += 1
            return f"ct-{self._counter}"

    async def handle_response(self, msg):
        rid = msg.get("id")
        fut = self._pending.pop(rid, None)
        if fut is not None and not fut.done():
            fut.set_result(msg)

    def _fn_name(self, server, tool):
        # Mirror the Rust codegen fn name (server_tool, sanitized). The mutation
        # set is keyed by the generated Python fn name.
        def san(x):
            return "".join(c if (c.isalnum() or c == "_") else "_" for c in x)
        return f"{san(server)}_{san(tool)}"

    def call_tool(self, server, tool, args):
        """Fire a `call_tool` request and return a lazily-resolved `Pending`.

        Honours per-run dry-run (mutating calls are stubbed, reads still go to the
        wire) and records every call into the run's trace sink.
        """
        is_mutation = self._fn_name(server, tool) in self._mutations

        # Dry-run: intercept mutating calls; return a deterministic stub instead
        # of hitting the upstream. Reads still execute so dependent logic works.
        if self._dry_run and is_mutation:
            if self._trace is not None:
                self._trace.record(
                    server=server, tool=tool, args=args, ok=True,
                    kind="dry_run", elapsed_ms=0, mutation=True,
                )
            return _DryRunResult(server, tool, args)

        # Extract universal control kwargs BEFORE sending so upstream never
        # sees them as unknown tool arguments.
        timeout = None
        if isinstance(args, dict) and "timeout_ms" in args:
            try:
                timeout = float(args.pop("timeout_ms")) / 1000.0
            except (TypeError, ValueError):
                args.pop("timeout_ms", None)
                timeout = None

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
        return Pending(
            fut, self._loop, server, tool,
            args=args, trace=self._trace, timeout=timeout, mutation=is_mutation,
        )


class _DryRunResult(dict):
    """Deterministic stand-in returned for a mutating call under dry-run.

    Subclasses ``dict`` so downstream code that reads common keys (``id``,
    ``number``, ``name``, ``url``, ``sha``, ``key``) gets a stable stub value and
    can proceed, while a marker records that nothing actually mutated.
    """

    def __init__(self, server, tool, args):
        stub = f"<dry-run:{server}/{tool}>"
        super().__init__({
            "_dry_run": True,
            "server": server,
            "tool": tool,
            "args": _redact_args(args if isinstance(args, dict) else {}),
            "id": stub,
            "number": 0,
            "name": stub,
            "url": stub,
            "sha": stub,
            "key": stub,
        })


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
_RUNTIME_INJECTED_NAMES = frozenset(
    {"Pending", "ToolError", "result", "gather", "resolve", "settle"}
)

# Universal SDK kwargs accepted by every generated function at runtime (handled
# by the dispatcher, not part of the tool's own schema). The validator must not
# flag these as unknown.
_UNIVERSAL_KWARGS = frozenset({"timeout_ms"})

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
        if kw.arg in _UNIVERSAL_KWARGS:
            # Accepted by every SDK function at runtime (e.g. timeout_ms).
            continue
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


def _sdk_mutations(sdk_module):
    """Set of generated fn names classified as mutating (write) tools."""
    m = getattr(sdk_module, "_codemcp_mutations", None)
    if isinstance(m, (set, frozenset, list, tuple)):
        return set(m)
    return set()


def _validate_user_code(
    code,
    sdk_module,
    allow_mutations=None,
    dry_run=False,
    keysets=None,
    enforce_mutations=False,
):
    """Statically validate `code` against the SDK contract.

    Returns ``None`` if the code passes the pre-flight checks, otherwise a
    structured, human-readable error string describing each problem found.

    The mutation gate is **opt-in**: it only runs when ``enforce_mutations`` is
    true (operator setting `CODEMCP_ENFORCE_MUTATIONS`). By default writes are
    allowed and this check is skipped entirely.

    ``allow_mutations`` is the set of mutating SDK fn names the caller explicitly
    authorized for this run. When enforcement is on, any mutating call not in that
    set is rejected before execution, so a write can't happen without the model
    naming it.

    When ``dry_run`` is true the mutation-budget gate is skipped even under
    enforcement: no upstream writes will happen anyway (the dispatcher stubs
    mutating calls), so the validator lets the code through so the model can
    preview it.

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
    mutations = _sdk_mutations(sdk_module)
    allowed = set(allow_mutations or ())
    # The gate is opt-in (operator's CODEMCP_ENFORCE_MUTATIONS). It's also skipped
    # under dry_run, where the dispatcher stubs every mutating call so no write
    # happens anyway. Default: no enforcement, writes are allowed.
    gate_writes = bool(enforce_mutations) and not dry_run

    errors = []
    undeclared_mutations = set()
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call) or not isinstance(node.func, ast.Name):
            continue
        name = node.func.id

        if name in sdk_fns:
            _validate_call(node, name, sdk_fns[name], errors)
            # Mutation budget: when enforcement is on, a write tool must be
            # explicitly authorized.
            if gate_writes and name in mutations and name not in allowed:
                undeclared_mutations.add(name)
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

    for name in sorted(undeclared_mutations):
        errors.append(
            _format_call_error(
                name,
                "is a mutating (write) tool and was not authorized.",
                "Re-send with this call listed in `allow_mutations`, or use "
                "`dry_run: true` to preview without writing.",
            )
        )

    # Strict return-field validation (only when the gateway shipped keysets).
    if keysets:
        _validate_field_access(tree, sdk_fns, keysets, errors)

    if not errors:
        return None

    header = (
        "Pre-flight validation failed (code was not executed). "
        "Fix these and resend:\n"
    )
    return header + "\n".join(f"  - {e}" for e in errors)


def _gather(*pendings, timeout=None):
    """Resolve many calls concurrently without raising (the allSettled shape).

    Accepts ``Pending`` handles (or already-computed values) and returns a list
    of ``{"ok": True, "value": ...}`` / ``{"ok": False, "error": {...}}`` in the
    same order. One failing call never aborts the batch.
    """
    if len(pendings) == 1 and isinstance(pendings[0], (list, tuple)):
        pendings = tuple(pendings[0])
    results = []
    for p in pendings:
        if isinstance(p, Pending):
            results.append(p.settled(timeout))
        else:
            results.append({"ok": True, "value": p})
    return results


def _resolve(value, _depth=0):
    """Deep-resolve any ``Pending`` nested in lists/dicts/tuples/sets."""
    if _depth > 64:
        return value
    if isinstance(value, Pending):
        value = value.result()
    if isinstance(value, dict):
        return {k: _resolve(v, _depth + 1) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        seq = [_resolve(v, _depth + 1) for v in value]
        return type(value)(seq) if isinstance(value, tuple) else seq
    if isinstance(value, set):
        return {_resolve(v, _depth + 1) for v in value}
    return value


def _last_expression_name(tree):
    """If the module's final statement is a bare expression, return a synthetic
    assignment target so we can capture its value as the result."""
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        return tree.body[-1]
    return None


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


# Attribute names that address the `Pending` wrapper itself, not the result. A
# variable bound to `x = sdk_fn(...)` is a `Pending`: indexing/attribute access
# transparently resolves to the value, but these names call wrapper methods
# (e.g. `x.result()`, `x.settled()`).
_PENDING_API_NAMES = frozenset(
    {
        "result",
        "settled",
        "value",
        "exception",
        "as_dict",
        "done",
        "cancel",
        "cancelled",
    }
)

# Builtin methods of the containers a resolved tool result is made of (dict,
# list, str). Attribute access with these names is a method call on the value
# (e.g. `data["details"].get("bio")`, `items.append(...)`), never a result
# field — so field-validation must not treat them as unknown keys. Applies at
# ANY depth in an access chain, not just the first step.
_CONTAINER_METHOD_NAMES = frozenset(
    dir(dict) + dir(list) + dir(str) + dir(tuple) + dir(set)
)

# Any attribute name that should short-circuit field validation when reached via
# attribute (not subscript) access: wrapper API + container builtins.
_NON_FIELD_ATTR_NAMES = _PENDING_API_NAMES | _CONTAINER_METHOD_NAMES


def _literal_access_chain(node):
    """Peel a chain of literal subscripts/attributes off the OUTERMOST expr.

    Given an AST node that is the *target* of access (e.g. the whole
    `x["items"][0]["login"]`), return `(root_name, [steps...])` where root_name
    is the base `ast.Name` id and each step is a typed tuple:
      - ``("sub", "<field>")``  — literal string subscript `x["field"]`
      - ``("attr", "<name>")``  — attribute access `x.name`
      - ``("index", None)``     — literal integer subscript `x[0]`
    Returns (None, None) if the chain isn't a pure literal access on a Name
    (e.g. it uses a variable/slice subscript).
    """
    steps = []
    cur = node
    while True:
        if isinstance(cur, ast.Subscript):
            sl = cur.slice
            # Py3.9+: slice is the expression directly.
            if isinstance(sl, ast.Constant) and isinstance(sl.value, str):
                steps.append(("sub", sl.value))  # field key
                cur = cur.value
                continue
            if isinstance(sl, ast.Constant) and isinstance(sl.value, int) and not isinstance(
                sl.value, bool
            ):
                steps.append(("index", None))  # array index
                cur = cur.value
                continue
            return (None, None)  # non-literal / variable / slice -> bail
        if isinstance(cur, ast.Attribute):
            steps.append(("attr", cur.attr))
            cur = cur.value
            continue
        if isinstance(cur, ast.Name):
            steps.reverse()
            return (cur.id, steps)
        return (None, None)


def _render_path(steps):
    """Render the access path taken so far for an error message.

    `[("sub","items"), ("index",None)]` -> `['items'][0]`; empty -> `result`.
    """
    if not steps:
        return "result"
    out = ""
    for kind, val in steps:
        if kind == "index":
            out += "[0]"
        elif kind == "attr":
            out += f".{val}"
        else:  # "sub"
            out += f"[{val!r}]"
    return out


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
        root, steps = _literal_access_chain(node)
        if root is None or root not in tracked or not steps:
            continue

        fn_name = tracked[root]
        ks = keysets.get(fn_name)
        lineno = getattr(node, "lineno", 0)
        # Descend step by step; stop at first undecidable or invalid step.
        for depth, (kind, step) in enumerate(steps):
            if kind == "index":
                # Index into an array: descend into the element keyset. If this
                # node isn't an array (polymorphic/opaque), stop validating.
                if _keyset_kind(ks) == "array":
                    ks = ks.get("items") or {"k": "leaf"}
                    continue
                break
            if kind == "attr" and step in _NON_FIELD_ATTR_NAMES:
                # `x.result()`, `data["a"].get(...)`, `items.append(...)`: an
                # attribute call on the Pending wrapper or on a dict/list/str
                # value — not a result field. Stop validating this chain.
                break
            ok, info = _keyset_descend(ks, step)
            if ok is None:
                break  # can't validate here (leaf/opaque) -> stop
            if ok is False:
                valid = info
                where = f"(line {lineno})"
                near = difflib.get_close_matches(step, valid, n=1, cutoff=_SUGGEST_CUTOFF)
                path = _render_path(steps[:depth])
                hint = (
                    f"Did you mean `{near[0]}`?"
                    if near
                    else f"Available fields: {', '.join(valid)}."
                )
                errors.append(
                    _format_call_error(
                        fn_name,
                        f"result{('' if path == 'result' else path)} has no field "
                        f"`{step}` {where}.",
                        hint,
                    )
                )
                break
            ks = info  # descend


def _exec_user_code(code, sdk_module, dispatcher, options=None, keysets=None):
    """Execute user code, returning (result, stdout, stderr, error, trace, mutations)."""
    options = options or {}
    allow_mutations = options.get("allow_mutations") or []
    dry_run = bool(options.get("dry_run"))
    enforce_mutations = bool(options.get("enforce_mutations"))

    # Pre-flight: reject statically-broken code before running anything, so the
    # model gets a precise hint instead of a post-hoc traceback.
    validation_error = _validate_user_code(
        code,
        sdk_module,
        allow_mutations,
        dry_run,
        keysets,
        enforce_mutations=enforce_mutations,
    )
    if validation_error is not None:
        return None, "", "", validation_error, [], []

    trace = _TraceSink()
    dispatcher.begin_run(
        trace=trace, dry_run=dry_run, mutations=_sdk_mutations(sdk_module)
    )

    namespace = {"__name__": "__codemcp__"}
    # Inject SDK functions directly — each returns a Pending when called.
    for name in dir(sdk_module):
        if not name.startswith("_"):
            namespace[name] = getattr(sdk_module, name)

    # Wire dispatch into the SDK module.
    sdk_module._codemcp_dispatch = dispatcher.call_tool

    namespace["Pending"] = Pending
    namespace["ToolError"] = ToolError
    namespace["gather"] = _gather
    namespace["settle"] = _gather  # alias
    namespace["resolve"] = _resolve
    # Remember the loop so Pending can reach it from this thread.
    _thread_local._loop = dispatcher._loop

    out, err = io.StringIO(), io.StringIO()
    result_value = None
    error = None
    try:
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            tree = ast.parse(code, filename="<codemcp>", mode="exec")
            # Support returning the final bare expression implicitly (so a
            # one-liner needs no `result =`), unless `result` is set explicitly.
            tail = _last_expression_name(tree)
            assigns_result = "result" in _collect_bound_names(tree)
            if tail is not None and not assigns_result:
                capture = ast.Assign(
                    targets=[ast.Name(id="result", ctx=ast.Store())],
                    value=tail.value,
                )
                ast.copy_location(capture, tail)
                tree.body[-1] = capture
                ast.fix_missing_locations(tree)
            compiled = compile(tree, "<codemcp>", "exec")
            exec(compiled, namespace)
            # Convention: `result` variable, else None.
            result_value = namespace.get("result")
            # Deep-resolve so any lingering Pending (including nested) is fetched
            # before serialization — the agent never has to force resolution.
            result_value = _resolve(result_value)
    except ToolError as e:
        # Uncaught tool failure: emit a structured, localized error instead of a
        # raw traceback so the model knows exactly which call/args failed.
        error = json.dumps({"tool_error": e.as_dict()})
    except Exception:
        error = traceback.format_exc()

    return (
        result_value,
        out.getvalue(),
        err.getvalue(),
        error,
        trace.trace(),
        trace.mutations(),
    )


def _json_safe(value, _depth=0):
    """Recursively make a value JSON-serializable, resolving any Pending."""
    if _depth > 64:
        return repr(value)
    if isinstance(value, Pending):
        value = value.result()
    if isinstance(value, dict):
        return {str(k): _json_safe(v, _depth + 1) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_json_safe(v, _depth + 1) for v in value]
    if isinstance(value, (set, frozenset)):
        return [_json_safe(v, _depth + 1) for v in value]
    try:
        json.dumps(value)
        return value
    except (TypeError, ValueError):
        return repr(value)


# Fallback byte cap used when the gateway does not inject one (`max_output_bytes`
# == 0). Mirrors the Rust-side default of 1 MiB so behavior is identical whether
# or not the setting is threaded through.
_DEFAULT_MAX_OUTPUT_BYTES = 1_048_576


def _truncate_text(text, limit):
    """Cap a stdout/stderr string at `limit` UTF-8 bytes, appending a marker that
    names how many bytes were dropped so the model knows output was clipped."""
    if not text or limit <= 0:
        return text
    encoded = text.encode("utf-8", "replace")
    if len(encoded) <= limit:
        return text
    dropped = len(encoded) - limit
    clipped = encoded[:limit].decode("utf-8", "ignore")
    return f"{clipped}\n…[truncated {dropped} bytes; raise CODEMCP_MAX_OUTPUT_BYTES or return a summary]"


def _cap_result(value, limit):
    """Bound a JSON-safe result at `limit` bytes. If the serialized value fits,
    return it unchanged. Otherwise replace it with a compact, actionable envelope
    so the model gets a signal + a hint instead of a wall of text (or a payload
    that never should have crossed the wire)."""
    if limit <= 0:
        return value
    try:
        encoded = json.dumps(value).encode("utf-8", "replace")
    except (TypeError, ValueError):
        encoded = repr(value).encode("utf-8", "replace")
    if len(encoded) <= limit:
        return value
    # Keep a small textual preview well under the cap so the envelope itself is
    # never oversized.
    preview_bytes = min(2048, max(0, limit - 256))
    preview = encoded[:preview_bytes].decode("utf-8", "ignore")
    return {
        "_truncated": True,
        "bytes": len(encoded),
        "limit": limit,
        "preview": preview,
        "hint": (
            "result exceeded CODEMCP_MAX_OUTPUT_BYTES; return a summary, select "
            "specific fields, or write the full data to a file instead of "
            "returning it verbatim"
        ),
    }


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
    params = msg.get("params", {}) or {}
    code = params.get("code", "")
    rid = msg.get("id")
    options = {
        "allow_mutations": params.get("allow_mutations") or [],
        "dry_run": bool(params.get("dry_run")),
        "enforce_mutations": bool(params.get("enforce_mutations")),
    }
    # Operator byte cap for the payload sent back to the model. `0`/absent means
    # the gateway didn't inject one, so fall back to the built-in default rather
    # than shipping an unbounded result.
    try:
        cap = int(params.get("max_output_bytes") or 0)
    except (TypeError, ValueError):
        cap = 0
    if cap <= 0:
        cap = _DEFAULT_MAX_OUTPUT_BYTES
    # Run user code off the event loop so SDK calls can round-trip.
    result_value, stdout, stderr, error, trace, mutations = await asyncio.to_thread(
        _exec_user_code, code, holder.module, dispatcher, options, holder.keysets
    )
    # Bound every model-facing field before serialization so a runaway result or
    # `print` can't blow the token budget or flood the transport.
    response = {
        "jsonrpc": "2.0",
        "id": rid,
        "result": {
            "result": _cap_result(_json_safe(result_value), cap),
            "stdout": _truncate_text(stdout, cap),
            "stderr": _truncate_text(stderr, cap),
            "error": error,
            "trace": trace,
            "mutations": mutations,
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

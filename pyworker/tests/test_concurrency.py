#!/usr/bin/env python3
"""Self-test for the worker's Pending / concurrent execution model.

Runs entirely offline: it stands up a real asyncio event loop in a background
thread (mirroring the worker's main loop) and a fake Dispatcher whose responses
resolve after a simulated network delay. It then drives `_exec_user_code` exactly
as the real worker does, asserting:

  1. Sequential calls return real values transparently (a Pending resolves on use).
  2. Calls issued before any result is read overlap (wall time ≈ one delay).
  3. Reading each result before the next call is made serializes (control case).
  4. Tool errors surface as ToolError.
  5. A top-level `result` that is still a Pending is resolved before return.
  6. List comprehensions that issue then read run concurrently.

Run:  python3 pyworker/tests/test_concurrency.py
Exits non-zero on failure.
"""

import asyncio
import os
import sys
import threading
import types

# Avoid the websockets self-install path during import.
os.environ.setdefault("CODEMCP_WS_AUTO_INSTALL", "false")
sys.modules.setdefault("websockets", types.ModuleType("websockets"))

HERE = os.path.dirname(os.path.abspath(__file__))
PYWORKER = os.path.dirname(HERE)
sys.path.insert(0, PYWORKER)

import bootstrap  # noqa: E402

DELAY = 0.20  # simulated per-call network latency


class FakeDispatcher:
    """Mimics Dispatcher.call_tool but resolves futures after DELAY seconds.

    Each call returns a real bootstrap.Pending wrapping a loop future; the future
    is completed by a delayed callback on the loop, so concurrency is genuine.
    Honours per-run trace/dry-run/mutation state exactly like the real one.
    """

    def __init__(self, loop):
        self._loop = loop
        self._trace = None
        self._dry_run = False
        self._mutations = frozenset()

    def begin_run(self, trace, dry_run, mutations):
        self._trace = trace
        self._dry_run = bool(dry_run)
        self._mutations = frozenset(mutations or ())

    def _fn_name(self, server, tool):
        def san(x):
            return "".join(c if (c.isalnum() or c == "_") else "_" for c in x)
        return f"{san(server)}_{san(tool)}"

    def call_tool(self, server, tool, args):
        is_mutation = self._fn_name(server, tool) in self._mutations
        if self._dry_run and is_mutation:
            if self._trace is not None:
                self._trace.record(
                    server=server, tool=tool, args=args, ok=True,
                    kind="dry_run", elapsed_ms=0, mutation=True,
                )
            return bootstrap._DryRunResult(server, tool, args)

        fut = self._loop.create_future()
        timeout = None
        if isinstance(args, dict) and "timeout_ms" in args:
            timeout = float(args.pop("timeout_ms")) / 1000.0

        async def _resolve():
            await asyncio.sleep(DELAY)
            if tool == "boom":
                fut.set_result({"error": "simulated failure"})
            else:
                # Echo args back as the structured result.
                fut.set_result(
                    {"result": {"structuredContent": {"server": server, "tool": tool, "args": args}}}
                )

        asyncio.run_coroutine_threadsafe(_resolve(), self._loop)
        return bootstrap.Pending(
            fut, self._loop, server, tool,
            args=args, trace=self._trace, timeout=timeout, mutation=is_mutation,
        )


def make_sdk_module(dispatcher):
    """Build a fake sdk module exposing t1, t2, t3, a failing boom(), and a
    mutating write() tool (registered in _codemcp_mutations).

    Mirrors the generated SDK shape: functions accept named tool params plus
    ``**_extra`` and forward universal kwargs (e.g. ``timeout_ms``) into the
    args dict so the dispatcher sees them exactly as the real codegen does.
    """
    mod = types.ModuleType("sdk")

    def _make(tool):
        def fn(**kwargs):
            # Real codegen builds _args from named params and forwards universal
            # kwargs from **_extra. Here every kwarg is treated as a tool param
            # for simplicity, but timeout_ms is a universal kwarg by contract.
            return mod._codemcp_dispatch("srv", tool, dict(kwargs))

        fn.__name__ = tool
        return fn

    mod.t1 = _make("t1")
    mod.t2 = _make("t2")
    mod.t3 = _make("t3")
    mod.boom = _make("boom")
    mod.write = _make("write")
    mod._codemcp_mutations = {"srv_write"}
    mod._codemcp_dispatch = dispatcher.call_tool
    return mod


def run_loop_in_thread():
    loop = asyncio.new_event_loop()
    ready = threading.Event()

    def _run():
        asyncio.set_event_loop(loop)
        loop.call_soon(ready.set)
        loop.run_forever()

    t = threading.Thread(target=_run, daemon=True)
    t.start()
    ready.wait()
    return loop, t


def exec_code(code, sdk_module, dispatcher, options=None):
    # Return (result, stdout, stderr, error) for the legacy call sites; the new
    # trace/mutations are exercised via exec_full below.
    r, out, err, error, _trace, _muts = bootstrap._exec_user_code(
        code, sdk_module, dispatcher, options
    )
    return r, out, err, error


def exec_full(code, sdk_module, dispatcher, options=None):
    return bootstrap._exec_user_code(code, sdk_module, dispatcher, options)


FAILS = []


def check(name, cond, detail=""):
    status = "ok" if cond else "FAIL"
    print(f"  [{status}] {name}{(' — ' + detail) if detail else ''}")
    if not cond:
        FAILS.append(name)


def main():
    loop, _thread = run_loop_in_thread()
    dispatcher = FakeDispatcher(loop)
    sdk_module = make_sdk_module(dispatcher)

    print("1. sequential value is transparent")
    result, out, err, error = exec_code(
        "result = t1(x=1)['args']['x']", sdk_module, dispatcher
    )
    check("no error", error is None, error or "")
    check("indexed value resolves to 1", result == 1, repr(result))

    print("2. issue-then-read overlaps round-trips")
    code = """
import time
start = time.time()
a = t1(x=1)        # fired
b = t2(y=2)        # fired
c = t3(z=3)        # fired
# now read — all three already in flight
va, vb, vc = a["tool"], b["tool"], c["tool"]
elapsed = time.time() - start
result = {"vals": [va, vb, vc], "elapsed": elapsed}
"""
    result, out, err, error = exec_code(code, sdk_module, dispatcher)
    check("no error", error is None, error or "")
    check("results in order", result and result["vals"] == ["t1", "t2", "t3"], repr(result))
    check(
        "elapsed ≈ one DELAY (concurrent, not 3x)",
        result and result["elapsed"] < DELAY * 2,
        f"elapsed={result['elapsed']:.3f}s vs sum={3 * DELAY:.3f}s",
    )

    print("3. read-before-next-call serializes (control)")
    code = """
import time
start = time.time()
a = t1(x=1)["tool"]   # blocks here
b = t2(y=2)["tool"]   # only fired after a resolved
elapsed = time.time() - start
result = elapsed
"""
    result, out, err, error = exec_code(code, sdk_module, dispatcher)
    check("no error", error is None, error or "")
    check(
        "elapsed ≈ 2x DELAY (serialized as expected)",
        result and result >= DELAY * 1.8,
        f"elapsed={result:.3f}s",
    )

    print("4. tool errors surface as ToolError")
    code = """
try:
    boom()["x"]
    result = "no-error"
except ToolError as e:
    result = "caught: " + str(e)
"""
    result, out, err, error = exec_code(code, sdk_module, dispatcher)
    check("no uncaught error", error is None, error or "")
    check("ToolError caught", isinstance(result, str) and result.startswith("caught:"), repr(result))

    print("5. top-level Pending result is resolved before return")
    result, out, err, error = exec_code("result = t1(x=42)", sdk_module, dispatcher)
    check("no error", error is None, error or "")
    check(
        "result resolved to dict, not Pending",
        isinstance(result, dict) and result.get("args", {}).get("x") == 42,
        repr(result),
    )

    print("6. comprehension issue-then-read runs concurrently")
    code = """
import time
start = time.time()
pages = [t1(x=i) for i in range(5)]   # all five fired
xs = [p["args"]["x"] for p in pages]  # then read
elapsed = time.time() - start
result = {"xs": xs, "elapsed": elapsed}
"""
    result, out, err, error = exec_code(code, sdk_module, dispatcher)
    check("no error", error is None, error or "")
    check("five results in order", result and result["xs"] == [0, 1, 2, 3, 4], repr(result))
    check(
        "elapsed ≈ one DELAY (concurrent, not 5x)",
        result and result["elapsed"] < DELAY * 2,
        f"elapsed={result['elapsed']:.3f}s vs sum={5 * DELAY:.3f}s",
    )

    print("7. gather() resolves many calls without raising (allSettled)")
    code = """
result = gather(t1(x=1), boom(), t2(y=2))
"""
    result, out, err, error = exec_code(code, sdk_module, dispatcher)
    check("no uncaught error", error is None, error or "")
    check("three settled entries", isinstance(result, list) and len(result) == 3, repr(result))
    check("first ok", result and result[0]["ok"] is True, repr(result[0]))
    check("second failed structured", result and result[1]["ok"] is False
          and result[1]["error"]["server"] == "srv", repr(result[1]))
    check("third ok", result and result[2]["ok"] is True, repr(result[2]))

    print("8. trace records every call with timing + ok flag")
    result, out, err, error, trace, muts = exec_full(
        "result = [t1(x=1)['tool'], t2(y=2)['tool']]", sdk_module, dispatcher
    )
    check("no error", error is None, error or "")
    check("two trace entries", len(trace) == 2, repr(trace))
    check("trace has server/tool/ok/elapsed",
          all({"server", "tool", "ok", "elapsed_ms"} <= set(e) for e in trace),
          repr(trace))
    check("all ok", all(e["ok"] for e in trace), repr(trace))

    print("9. uncaught ToolError becomes a structured error field")
    result, out, err, error, trace, muts = exec_full(
        "result = boom()['x']", sdk_module, dispatcher
    )
    check("structured error emitted", error is not None, repr(error))
    import json as _json
    parsed = None
    try:
        parsed = _json.loads(error)
    except Exception:
        pass
    check("error is json with tool_error",
          isinstance(parsed, dict) and "tool_error" in parsed, repr(error))
    check("localized to server/tool",
          parsed and parsed["tool_error"]["server"] == "srv"
          and parsed["tool_error"]["tool"] == "boom", repr(parsed))
    check("trace records the failure",
          any(not e["ok"] for e in trace), repr(trace))

    print("10. dry_run stubs mutating calls, records mutation, does not dispatch")
    result, out, err, error, trace, muts = exec_full(
        "result = write(title='x')", sdk_module, dispatcher, {"dry_run": True}
    )
    check("no error", error is None, error or "")
    check("stub returned", isinstance(result, dict) and result.get("_dry_run") is True, repr(result))
    check("mutation audited", len(muts) == 1 and muts[0]["tool"] == "write", repr(muts))

    print("10b. dry_run bypasses the mutation-budget validator gate")
    # No allow_mutations declared, but dry_run must let it through.
    result, out, err, error, trace, muts = exec_full(
        "result = write(title='x')", sdk_module, dispatcher,
        {"dry_run": True},  # deliberately no allow_mutations
    )
    check("no error", error is None, repr(error))
    check("stub returned under dry_run", isinstance(result, dict) and result.get("_dry_run"), repr(result))

    print("11. authorized mutation actually dispatches (non-dry-run)")
    result, out, err, error, trace, muts = exec_full(
        "result = write(title='x')['tool']", sdk_module, dispatcher,
        {"allow_mutations": ["srv_write"]},
    )
    check("no error", error is None, error or "")
    check("dispatched real call", result == "write", repr(result))
    check("mutation audited as performed", len(muts) == 1 and muts[0]["ok"] is True, repr(muts))

    print("12. recursive resolve of nested Pending in result container")
    code = """
result = {"a": t1(x=1), "items": [t2(y=2), t3(z=3)]}
"""
    result, out, err, error = exec_code(code, sdk_module, dispatcher)
    check("no error", error is None, error or "")
    check("nested dict Pending resolved",
          isinstance(result["a"], dict) and result["a"]["tool"] == "t1", repr(result.get("a")))
    check("nested list Pending resolved",
          result["items"][0]["tool"] == "t2" and result["items"][1]["tool"] == "t3",
          repr(result.get("items")))

    print("13. per-call timeout_ms surfaces a structured timeout ToolError")
    code = """
try:
    # DELAY is 0.2s; a 1ms deadline must trip.
    t1(x=1, timeout_ms=1)["tool"]
    result = "no-timeout"
except ToolError as e:
    result = {"kind": e.kind, "tool": e.tool}
"""
    result, out, err, error = exec_code(code, sdk_module, dispatcher)
    check("no uncaught error", error is None, error or "")
    check("timeout classified",
          isinstance(result, dict) and result.get("kind") == "timeout", repr(result))

    # Let any in-flight delayed resolvers (e.g. behind the timeout test, whose
    # Pending we abandoned) finish on the loop before we stop it, so teardown is
    # quiet. The loop runs on a daemon thread and dies with the process.
    import time as _t
    _t.sleep(DELAY + 0.05)
    loop.call_soon_threadsafe(loop.stop)

    print()
    if FAILS:
        print(f"FAILED: {len(FAILS)} check(s): {', '.join(FAILS)}")
        sys.exit(1)
    print("All checks passed.")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Self-test for the worker's pre-flight static validator.

Runs entirely offline. It builds a fake SDK module whose functions have *real*
named signatures (mirroring the generated `sdk.py`, not `**kwargs`), then drives
`bootstrap._validate_user_code` / `bootstrap._exec_user_code` and asserts:

  1. Valid code passes (returns None).
  2. Syntax errors are reported with a line number, code not executed.
  3. A call to a misspelled SDK function is flagged with a suggestion.
  4. An unknown keyword argument is flagged (with a suggestion when close).
  5. A missing required argument is flagged.
  6. Legitimate dynamic Python (local defs, builtins, attribute calls,
     comprehensions, **kwargs spreads) is NOT false-flagged.
  7. Positional args are accounted for when checking required/extra args.
  8. _exec_user_code short-circuits on a validation error (no execution).

Run:  python3 pyworker/tests/test_validation.py
Exits non-zero on failure.
"""

import os
import sys
import types

# Avoid the websockets self-install path during import.
os.environ.setdefault("CODEMCP_WS_AUTO_INSTALL", "false")
sys.modules.setdefault("websockets", types.ModuleType("websockets"))

HERE = os.path.dirname(os.path.abspath(__file__))
PYWORKER = os.path.dirname(HERE)
sys.path.insert(0, PYWORKER)

import bootstrap  # noqa: E402


def make_sdk_module():
    """Fake sdk module whose functions have real named signatures.

    Mirrors the shape of the generated sdk.py: required params first, optional
    params with `= None` defaults. The bodies don't matter — the validator only
    inspects signatures via `inspect.signature`.
    """
    mod = types.ModuleType("sdk")

    def github_get_pull_request(owner, repo, pullNumber):  # noqa: N803
        return None

    def github_search_issues(query, state=None, perPage=None):  # noqa: N803
        return None

    def github_create_issue(owner, repo, title, body=None):
        return None

    # A private/dunder name must be ignored by the contract scan.
    def _codemcp_dispatch(server, tool, args):
        return None

    mod.github_get_pull_request = github_get_pull_request
    mod.github_search_issues = github_search_issues
    mod.github_create_issue = github_create_issue
    mod._codemcp_dispatch = _codemcp_dispatch
    return mod


class _NoopDispatcher:
    # `_exec_user_code` stashes `dispatcher._loop` for Pending resolution; we
    # never dispatch (the test functions return plain values), so None is fine.
    _loop = None

    def call_tool(self, server, tool, args):
        raise AssertionError("dispatcher must not be called in these tests")


FAILS = []


def check(name, cond, detail=""):
    status = "ok" if cond else "FAIL"
    print(f"  [{status}] {name}{(' — ' + detail) if detail else ''}")
    if not cond:
        FAILS.append(name)


def main():
    sdk = make_sdk_module()
    V = bootstrap._validate_user_code

    print("1. valid code passes")
    err = V("issue = github_search_issues(query='x', state='open')\nresult = issue", sdk)
    check("returns None", err is None, repr(err))

    print("2. syntax error reported, not executed")
    err = V("result = github_search_issues(query='x'", sdk)  # missing paren
    check("flagged", err is not None)
    check("mentions SyntaxError", err and "SyntaxError" in err, repr(err))
    check("has a line number", err and "line" in err, repr(err))

    print("3. misspelled SDK function suggests the real name")
    err = V("result = github_serch_issues(query='x')", sdk)  # serch -> search
    check("flagged", err is not None)
    check(
        "suggests github_search_issues",
        err and "github_search_issues" in err,
        repr(err),
    )

    print("4. unknown keyword argument is flagged")
    err = V("result = github_search_issues(query='x', stat='open')", sdk)  # stat -> state
    check("flagged", err is not None)
    check("names the bad kwarg", err and "stat" in err, repr(err))
    check("suggests `state`", err and "state" in err, repr(err))

    print("4b. truly unknown kwarg lists valid args")
    err = V("result = github_search_issues(query='x', zzz=1)", sdk)
    check("flagged", err is not None and "zzz" in err, repr(err))

    print("5. missing required argument is flagged")
    err = V("result = github_get_pull_request(owner='o', repo='r')", sdk)
    check("flagged", err is not None)
    check("names the missing arg", err and "pullNumber" in err, repr(err))

    print("6. dynamic / legitimate Python is not false-flagged")
    cases = {
        "local function def + call": """
def helper(a, b):
    return a + b
result = helper(1, 2)
""",
        "builtin calls": "result = len([1, 2, 3]) + int('4') + max(1, 2)",
        "attribute method call": "s = 'hello'\nresult = s.upper().strip()",
        "comprehension target": "result = [github_search_issues(query=str(i)) for i in range(3)]",
        "lambda + map": "result = list(map(lambda x: x * 2, [1, 2, 3]))",
        "kwargs spread (cannot resolve statically)": """
opts = {'query': 'x', 'state': 'open'}
result = github_search_issues(**opts)
""",
        "imported name call": "import json\nresult = json.dumps({'a': 1})",
        "positional args correct": "result = github_get_pull_request('o', 'r', 5)",
    }
    for label, code in cases.items():
        err = V(code, sdk)
        check(f"not flagged: {label}", err is None, repr(err))

    print("7. extra positional args are flagged")
    err = V("result = github_get_pull_request('o', 'r', 5, 6)", sdk)
    check("flagged", err is not None, repr(err))

    print("7b. positional fills required (no false 'missing')")
    err = V("result = github_create_issue('o', 'r', 'title')", sdk)
    check("not flagged (body optional)", err is None, repr(err))

    print("8. _exec_user_code short-circuits on validation error")
    result, out, errs, error = bootstrap._exec_user_code(
        "result = github_serch_issues(query='x')", sdk, _NoopDispatcher()
    )
    check("error surfaced", error is not None and "validation" in error.lower(), repr(error))
    check("no result", result is None, repr(result))
    check("no stdout", out == "", repr(out))

    print("8b. valid code still executes through _exec_user_code")
    # Give the SDK function a body that returns a concrete value for this check.
    sdk.github_create_issue = lambda owner, repo, title, body=None: {"ok": True}
    result, out, errs, error = bootstrap._exec_user_code(
        "result = github_create_issue(owner='o', repo='r', title='t')",
        sdk,
        _NoopDispatcher(),
    )
    check("executed, no error", error is None, repr(error))
    check("returned value", result == {"ok": True}, repr(result))

    # ── 9. strict return-field validation (keysets shipped by the gateway) ──
    # Serialized KeySet form (matches src/sdk/keyset.rs serde):
    #   object -> {"k":"object","fields":{name: <keyset>, ...}}
    #   array  -> {"k":"array","items":<keyset>}
    #   leaf   -> {"k":"leaf"}
    def obj(**fields):
        return {"k": "object", "fields": fields}

    def arr(items):
        return {"k": "array", "items": items}

    LEAF = {"k": "leaf"}
    keysets = {
        # github_search_issues returns {issues: [{number, title, user: {login, id}}], totalCount}
        "github_search_issues": obj(
            issues=arr(obj(number=LEAF, title=LEAF, user=obj(login=LEAF, id=LEAF))),
            totalCount=LEAF,
        ),
        # github_get_pull_request returns {number, title, user: {login, id}}
        "github_get_pull_request": obj(
            number=LEAF, title=LEAF, user=obj(login=LEAF, id=LEAF)
        ),
    }

    def Vf(code):
        return V(code, sdk, keysets)

    print("9. strict return-field validation")

    print("9a. valid top-level field passes")
    err = Vf("pr = github_get_pull_request('o', 'r', 1)\nresult = pr['title']")
    check("valid field not flagged", err is None, repr(err))

    print("9b. typo in top-level field is flagged with suggestion")
    err = Vf("pr = github_get_pull_request('o', 'r', 1)\nresult = pr['titel']")
    check("flagged", err is not None, repr(err))
    check("suggests `title`", err and "title" in err, repr(err))
    check("not executed wording", err and "no field" in err, repr(err))

    print("9c. attribute-style typo is flagged")
    err = Vf("pr = github_get_pull_request('o', 'r', 1)\nresult = pr.titel")
    check("flagged", err is not None, repr(err))

    print("9d. nested valid field passes")
    err = Vf("pr = github_get_pull_request('o', 'r', 1)\nresult = pr['user']['login']")
    check("nested valid not flagged", err is None, repr(err))

    print("9e. nested typo is flagged")
    err = Vf("pr = github_get_pull_request('o', 'r', 1)\nresult = pr['user']['lgoin']")
    check("flagged", err is not None, repr(err))
    check("suggests `login`", err and "login" in err, repr(err))

    print("9f. dynamic/non-literal access is NOT flagged")
    err = Vf("pr = github_get_pull_request('o', 'r', 1)\nk='x'\nresult = pr[k]")
    check("variable subscript not flagged", err is None, repr(err))

    print("9g. cold tool (no keyset) is NOT flagged")
    err = Vf("x = github_create_issue('o','r','t')\nresult = x['nope']")
    check("no keyset -> no check", err is None, repr(err))

    print("9h. reassigned/poisoned name is NOT flagged")
    err = Vf(
        "pr = github_get_pull_request('o','r',1)\npr = {'whatever': 1}\nresult = pr['titel']"
    )
    check("poisoned name not validated", err is None, repr(err))

    print("9i. valid field after array index passes (descends through [0])")
    err = Vf(
        "r = github_search_issues(query='x')\nresult = r['issues'][0]['number']"
    )
    check("valid array-element field not flagged", err is None, repr(err))

    print("9i2. typo on a field after array index IS flagged")
    err = Vf(
        "r = github_search_issues(query='x')\nresult = r['issues'][0]['numbr']"
    )
    check("array-element typo flagged", err is not None, repr(err))
    check("suggests `number`", err and "number" in err, repr(err))
    check("path shows index", err and "['issues'][0]" in err, repr(err))

    print("9i3. nested obj-in-array-element typo is flagged")
    err = Vf(
        "r = github_search_issues(query='x')\nresult = r['issues'][0]['user']['lgoin']"
    )
    check("nested array-element typo flagged", err is not None, repr(err))
    check("suggests `login`", err and "login" in err, repr(err))

    print("9j. typo on the array container key IS flagged")
    err = Vf("r = github_search_issues(query='x')\nresult = r['issuez']")
    check("flagged", err is not None, repr(err))
    check("suggests `issues`", err and "issues" in err, repr(err))

    print("9k. no keysets at all -> field checking inert")
    err = V("pr = github_get_pull_request('o','r',1)\nresult = pr['titel']", sdk, None)
    check("None keysets -> no field check", err is None, repr(err))

    print()
    if FAILS:
        print(f"FAILED: {len(FAILS)} check(s): {', '.join(FAILS)}")
        sys.exit(1)
    print("All checks passed.")


if __name__ == "__main__":
    main()

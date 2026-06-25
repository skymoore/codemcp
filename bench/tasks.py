"""Bench task definitions.

Each task is a read-only GitHub question over the `skymoore` user's data,
designed to exercise a known number of dependent tool calls so the two arms
(direct MCP vs codemcp) differ predictably in model round-trips.

  A  - 1 tool call  (baseline; isolates per-turn schema overhead)
  B  - 2 tool calls (search -> list_commits; tests round-trip collapse)
  C  - 2 tool calls (search -> get_file_contents; tests collapse + transform)

Deeper tasks (D/E/F) require indexing into NESTED fields of returned objects
whose names are not obvious from the tool signature (`-> dict[str, Any]`). They
are where shape-guessing bites: without a learned return shape the model must
guess keys like `owner.type`, `commit.author.name`, or `user.login`, and a wrong
guess costs a retry turn. These tasks isolate the value of the `codemcp_shapes`
arm.

  D  - 2 tool calls (search -> reuse repo object; nested owner/language fields)
  E  - 2 tool calls (search -> list_commits; deeply nested commit.author fields)
  F  - 2 tool calls (search -> list_issues; nested issue.user.login field)

The agent is free to take as many turns as it needs and to answer in natural
prose — no strict-JSON gate. Correctness is reviewed manually against
ground_truth.json (analyze.py shows every final answer next to the truth and
adds only a best-effort auto-flag). `answer_keys` are the values a human should
look for in each answer, and are also the targets for best-effort extraction.
"""

TASKS = [
    {
        "id": "A",
        "name": "repo_count",
        "tool_calls_expected": 1,
        "prompt": (
            "How many repositories does the GitHub user `skymoore` own in total? "
            "Use the available GitHub tools to find out (the search result's total "
            "count is the answer). Take as many steps as you need. When you have "
            "the answer, state it clearly and name the value explicitly, e.g. on "
            "its own line: `repo_count: 101`."
        ),
        "answer_keys": {"repo_count": int},
    },
    {
        "id": "B",
        "name": "most_starred_latest_commit",
        "tool_calls_expected": 2,
        "prompt": (
            "Find the GitHub user `skymoore`'s most-starred public repository. "
            "Then fetch that repository's single most recent commit and report the "
            "first line of its commit message. Take as many steps as you need. "
            "When you have the answer, state it clearly and name each value "
            "explicitly, e.g. on their own lines: `repo: owner/name`, "
            "`stars: 15`, `latest_commit_message: <first line>`."
        ),
        "answer_keys": {
            "repo": str,
            "stars": int,
            "latest_commit_message": str,
        },
    },
    {
        "id": "C",
        "name": "most_issues_readme",
        "tool_calls_expected": 2,
        "prompt": (
            "Find the GitHub user `skymoore`'s repository that has the most open "
            "issues. Then check whether that repository has a README file at the "
            "root of its default branch. Take as many steps as you need. When you "
            "have the answer, state it clearly and name each value explicitly, "
            "e.g. on their own lines: `repo: owner/name`, `open_issues: 20`, "
            "`has_readme: yes` (or `no`)."
        ),
        "answer_keys": {
            "repo": str,
            "open_issues": int,
            "has_readme": bool,
        },
    },
    {
        "id": "D",
        "name": "most_starred_owner_and_language",
        "tool_calls_expected": 1,
        "prompt": (
            "Find the GitHub user `skymoore`'s most-starred public repository. "
            "From that repository's data, report three things: the repository's "
            "full name, the account type of its owner (e.g. User or Organization), "
            "and its primary programming language. Take as many steps as you need. "
            "When you have the answer, state it clearly and name each value "
            "explicitly on its own line: `repo: owner/name`, `owner_type: User`, "
            "`language: Rust`. If the primary language is not set, use `none`."
        ),
        "answer_keys": {
            "repo": str,
            "owner_type": str,
            "language": str,
        },
    },
    {
        "id": "E",
        "name": "latest_commit_author",
        "tool_calls_expected": 2,
        "prompt": (
            "Find the GitHub user `skymoore`'s most-starred public repository, then "
            "fetch its single most recent commit. Report the repository's full "
            "name, the name of the person who authored that commit, and the date "
            "of that commit. Take as many steps as you need. When you have the "
            "answer, state it clearly and name each value explicitly on its own "
            "line: `repo: owner/name`, `author_name: <name>`, `commit_date: <date>`."
        ),
        "answer_keys": {
            "repo": str,
            "author_name": str,
            "commit_date": str,
        },
    },
    {
        "id": "F",
        "name": "most_issues_first_issue_author",
        "tool_calls_expected": 2,
        "prompt": (
            "Find the GitHub user `skymoore`'s repository with the most open "
            "issues. Then list that repository's open issues and identify the one "
            "with the lowest issue number. Report the repository's full name, that "
            "issue's number, and the login (username) of the account that opened "
            "it. Take as many steps as you need. When you have the answer, state it "
            "clearly and name each value explicitly on its own line: "
            "`repo: owner/name`, `issue_number: 12`, `issue_author: someuser`."
        ),
        "answer_keys": {
            "repo": str,
            "issue_number": int,
            "issue_author": str,
        },
    },
]

# System prompt prepended to every run. Identical for both arms so the only
# variable is how the GitHub toolset is exposed (~45 separate tools vs 1
# `execute_python` tool whose description lists ~45 signatures).
SYSTEM_PROMPT = (
    "You are a precise data-retrieval agent. Use the provided tools to answer "
    "the user's question about GitHub. Take as many steps as you need — there is "
    "no turn limit. When you have the answer, state it clearly in prose and "
    "explicitly name each requested value (on its own line, as `key: value`) so "
    "it can be verified. If a tool fails, retry once, then give the best answer "
    "you can."
)

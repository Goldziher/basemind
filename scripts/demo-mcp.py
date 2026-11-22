#!/usr/bin/env python3
"""Drive `basemind serve` over stdio the way an MCP host does, for the hero demo.

Usage: demo-mcp.py <basemind-binary> <repo-root> <outline-path> <symbol>

Sends `initialize`, lists the verb tools, then calls `code_outline` and
`code_refs`, printing each request and a trimmed answer.
"""

import json
import subprocess
import sys
import time

BIN, ROOT, PATH, SYMBOL = sys.argv[1:5]
CYAN, DIM, GREEN, RESET = "\033[36m", "\033[2m", "\033[1;32m", "\033[0m"
PAUSE = 1.2


def say(text=""):
    print(text, flush=True)


proc = subprocess.Popen(
    [BIN, "--root", ROOT, "serve"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL,
    text=True,
)
next_id = 0


def rpc(method, params=None, notify=False):
    global next_id
    msg = {"jsonrpc": "2.0", "method": method}
    if params is not None:
        msg["params"] = params
    if not notify:
        next_id += 1
        msg["id"] = next_id
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()
    if notify:
        return None
    while True:
        reply = json.loads(proc.stdout.readline())
        if reply.get("id") == next_id:
            return reply["result"]


def call(tool, args):
    say(f"{GREEN}tools/call{RESET} {CYAN}{tool}{RESET} {json.dumps(args)}")
    time.sleep(PAUSE / 2)
    result = rpc("tools/call", {"name": tool, "arguments": args})
    text = result["content"][0]["text"]
    try:
        answer = json.loads(text)
        lines = [f"total: {answer['total']}"] + [
            f"{path}: " + ", ".join(str(h["line"]) for h in hits) for path, hits in answer["hits"].items()
        ]
    except (ValueError, KeyError, TypeError):
        lines = text.splitlines()
    for line in lines[:12]:
        say(f"  {line[:96]}")
    if len(lines) > 12:
        say(f"{DIM}  ... {len(lines) - 12} more lines{RESET}")
    say(f"{DIM}  ({len(text)} bytes returned){RESET}")
    say()
    time.sleep(PAUSE)


init = rpc(
    "initialize",
    {
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": {"name": "demo-host", "version": "1"},
    },
)
rpc("notifications/initialized", notify=True)
tools = [t["name"] for t in rpc("tools/list")["tools"]]
say(f"{DIM}$ basemind serve    # MCP over stdio, {len(tools)} tools{RESET}")
for i in range(0, len(tools), 6):
    say(f"{DIM}  " + ", ".join(tools[i : i + 6]) + ("," if i + 6 < len(tools) else "") + RESET)
say()
time.sleep(PAUSE)
call("code_outline", {"path": PATH})
call("code_refs", {"name": SYMBOL, "limit": 6})
proc.stdin.close()
proc.terminate()

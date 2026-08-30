#!/usr/bin/env python3
"""Call ONE tool on the installed wicked-estate MCP server over stdio JSON-RPC.

The drill's verification MUST go through the exact read surface a governed worker
uses — the estate MCP's `rules.recall` / `knowledge.recall` tools — not through
this repo's own code. This helper spawns the server the way a worker harness does
(stdio, one graph --db, WICKED_KNOWLEDGE_DB for the knowledge sidecar), sends
initialize → initialized → tools/call, and prints the tool result's text content.

Usage: mcp-call.py <graph-db> <knowledge-db> <tool-name> '<json-args>'
Env:   WICKED_ESTATE_MCP_BIN (default: wicked-estate-mcp on PATH)
"""

import json
import os
import subprocess
import sys


def main() -> int:
    graph_db, knowledge_db, tool, raw_args = sys.argv[1:5]
    args = json.loads(raw_args)
    binary = os.environ.get("WICKED_ESTATE_MCP_BIN", "wicked-estate-mcp")

    requests = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "aw24-kill-switch-drill", "version": "0"},
            },
        },
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        },
    ]
    env = dict(os.environ, WICKED_KNOWLEDGE_DB=knowledge_db)
    proc = subprocess.run(
        [binary, "--db", graph_db],
        input="".join(json.dumps(r) + "\n" for r in requests),
        capture_output=True,
        text=True,
        timeout=60,
        env=env,
    )
    for line in proc.stdout.splitlines():
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("id") == 2:
            if "error" in msg:
                print(json.dumps(msg["error"]), file=sys.stderr)
                return 1
            for item in msg["result"].get("content", []):
                if item.get("type") == "text":
                    print(item["text"])
            return 0
    print("no tools/call response from the MCP server", file=sys.stderr)
    print(proc.stderr[-2000:], file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())

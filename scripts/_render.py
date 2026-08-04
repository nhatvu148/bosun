"""Render Kagoni's JSON-RPC replies for scripts/try.sh. Not part of the server."""

import json
import sys

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)

    if msg.get("id") == 1:
        info = msg["result"]["serverInfo"]
        print("connected: {} {}\n".format(info["name"], info["version"]))
        continue

    if msg.get("id") != 2:
        continue

    if "error" in msg:
        print("PROTOCOL ERROR:", msg["error"]["message"])
        sys.exit(1)

    result = msg["result"]

    # tools/list
    if "tools" in result:
        for tool in result["tools"]:
            hints = tool.get("annotations") or {}
            mark = "  <-- DESTRUCTIVE, needs dry_run or confirm" if hints.get(
                "destructiveHint"
            ) else ""
            print("  {:<22}{}".format(tool["name"], mark))
        print("\n{} tools".format(len(result["tools"])))
        sys.exit(0)

    # tools/call
    if result.get("isError"):
        print("TOOL ERROR:", result["content"][0]["text"])
        sys.exit(1)
    print(result["content"][0]["text"])

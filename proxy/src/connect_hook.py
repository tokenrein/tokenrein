#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tokenrein hook for coding agents whose hooks run commands (Codex CLI, Gemini CLI, Cursor).

The agent runs `python3 ~/.tokenrein/hook.py <tool>` with the hook event as JSON on stdin. This
forwards it to Tokenrein and prints Tokenrein's answer, which blocks the prompt or tool call when
a run is paused or a budget is used up. The URL and key come from ~/.tokenrein/<tool>.json,
written by the connect script. It never blocks the agent because Tokenrein is unreachable.
Standard library only.
"""

import json
import os
import sys
import urllib.request

TOOLS = {"codex", "gemini", "cursor"}


def allow(tool, event):
    """The answer that lets the action through, in the tool's own format."""
    if tool == "cursor":
        if event == "beforeSubmitPrompt":
            return {"continue": True}
        if event in ("preToolUse", "beforeShellExecution", "beforeMCPExecution", "subagentStart"):
            return {"permission": "allow"}
    return {}


def main():
    tool = sys.argv[1] if len(sys.argv) > 1 else ""
    raw = sys.stdin.buffer.read()
    try:
        event = json.loads(raw or b"{}").get("hook_event_name", "")
    except (ValueError, AttributeError):
        event = ""
    answer = allow(tool, event)
    try:
        if tool not in TOOLS:
            raise ValueError("unknown tool " + tool)
        home = os.path.join(os.path.expanduser("~"), ".tokenrein")
        with open(os.path.join(home, tool + ".json")) as f:
            cfg = json.load(f)
        headers = {
            "Authorization": "Bearer " + cfg["key"],
            "content-type": "application/json",
            "x-tr-agent": cfg.get("agent") or tool,
        }
        if cfg.get("user"):
            headers["x-tr-user"] = cfg["user"]
        req = urllib.request.Request(
            cfg["url"].rstrip("/") + "/v1/hooks/" + tool, data=raw, headers=headers, method="POST"
        )
        with urllib.request.urlopen(req, timeout=float(cfg.get("timeout", 4))) as r:
            body = json.loads(r.read() or b"{}")
        if isinstance(body, dict):
            answer = body
    except Exception as e:  # fail open: never stop the agent because Tokenrein is unreachable
        print("tokenrein hook: %s" % e, file=sys.stderr)
    sys.stdout.write(json.dumps(answer))
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())

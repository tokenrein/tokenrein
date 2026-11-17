#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tokenrein hook for coding agents whose hooks run commands (Codex CLI, Gemini CLI, Cursor,
GitHub Copilot CLI).

The agent runs `python3 ~/.tokenrein/hook.py <tool>` with the hook event as JSON on stdin. This
forwards it to Tokenrein and prints Tokenrein's answer, which blocks the prompt or tool call when
a run is paused or a budget is used up. The URL and key come from <tool>.json next to this file
(~/.tokenrein, or /etc/tokenrein for machine-wide Copilot policy hooks), written by the connect
script. An event name after the tool is added to payloads that don't name their event. It never blocks the agent because Tokenrein is unreachable.
Standard library only.
"""

import json
import os
import sys
import urllib.request

TOOLS = {"codex", "gemini", "cursor", "copilot"}


def allow(tool, event):
    """The answer that lets the action through, in the tool's own format."""
    if tool == "cursor":
        if event == "beforeSubmitPrompt":
            return {"continue": True}
        if event in ("preToolUse", "beforeShellExecution", "beforeMCPExecution", "subagentStart"):
            return {"permission": "allow"}
    return {}


TRANSCRIPT_TAIL_BYTES = 256 * 1024


def codex_result(doc):
    """How a Codex tool call ended, read from the session transcript on this machine.

    Codex's PostToolUse event carries only the command's output, so a failed command looks like a
    success. The transcript's `item_completed` entry for the call has its exit code and duration.
    Only those numbers are sent to Tokenrein, never the transcript. None when not found.
    """
    path, call_id = doc.get("transcript_path"), doc.get("tool_use_id")
    if not isinstance(path, str) or not isinstance(call_id, str) or not call_id:
        return None
    try:
        with open(path, "rb") as f:
            f.seek(0, os.SEEK_END)
            f.seek(max(0, f.tell() - TRANSCRIPT_TAIL_BYTES))
            lines = f.read().splitlines()
    except OSError:
        return None
    needle = call_id.encode()
    for line in reversed(lines):
        if needle not in line or b"item_completed" not in line:
            continue
        try:
            payload = json.loads(line).get("payload") or {}
        except (ValueError, AttributeError):
            continue
        item = payload.get("item") if isinstance(payload, dict) else None
        if payload.get("type") != "item_completed" or not isinstance(item, dict) or item.get("id") != call_id:
            continue
        result = {}
        if isinstance(item.get("exit_code"), int):
            result["exit_code"] = item["exit_code"]
        if isinstance(item.get("status"), str):
            result["status"] = item["status"]
        d = item.get("duration")
        if isinstance(d, dict) and isinstance(d.get("secs"), int):
            result["duration_ms"] = d["secs"] * 1000 + int(d.get("nanos") or 0) // 1000000
        return result or None
    return None


def main():
    tool = sys.argv[1] if len(sys.argv) > 1 else ""
    raw = sys.stdin.buffer.read()
    try:
        doc = json.loads(raw or b"{}")
        event = doc.get("hook_event_name", "")
        changed = False
        if not event and len(sys.argv) > 2:
            event = doc["hook_event_name"] = sys.argv[2]
            changed = True
        if tool == "codex" and event == "PostToolUse":
            result = codex_result(doc)
            if result:
                doc["tokenrein_result"] = result
                changed = True
        if changed:
            raw = json.dumps(doc).encode()
    except (ValueError, AttributeError, TypeError):
        event = ""
    answer = allow(tool, event)
    try:
        if tool not in TOOLS:
            raise ValueError("unknown tool " + tool)
        home = os.path.dirname(os.path.abspath(__file__))
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

#!/usr/bin/env python3
"""Connect Claude Code on this machine to TokenLens (or disconnect it).

Adds Claude Code's built-in usage telemetry and HTTP hooks to ~/.claude/settings.json, so every
new session reports its model calls (model, tokens, cost) to TokenLens and stops when an admin
pauses its run or an enforced budget is used up. Nothing is installed; the script only edits
that settings file, after saving a backup next to it. Standard library only.

    python3 claude-code.py --key tl_... --url http://127.0.0.1:8787
    python3 claude-code.py --remove
"""

import argparse
import json
import os
import shutil
import sys
import time
import urllib.error
import urllib.request

HOOK_PATH = "/v1/hooks/claude-code"
HOOK_EVENTS = ["UserPromptSubmit", "PreToolUse", "SubagentStart", "Stop", "SessionEnd"]
OUR_ENV = [
    "CLAUDE_CODE_ENABLE_TELEMETRY",
    "OTEL_LOGS_EXPORTER",
    "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
    "OTEL_LOGS_EXPORT_INTERVAL",
    "TOKENLENS_API_KEY",
]


def is_ours(group):
    return any(HOOK_PATH in h.get("url", "") for h in group.get("hooks", []))


def strip(settings):
    """Remove everything this script adds. Returns True if anything was removed."""
    changed = False
    env = settings.get("env", {})
    ours = env.get("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "").endswith("/v1/otel/v1/logs")
    if ours or "TOKENLENS_API_KEY" in env:
        for k in OUR_ENV:
            if k in env:
                del env[k]
                changed = True
        if not env:
            settings.pop("env", None)
    hooks = settings.get("hooks", {})
    for event in list(hooks):
        kept = [g for g in hooks[event] if not is_ours(g)]
        if len(kept) != len(hooks[event]):
            changed = True
            if kept:
                hooks[event] = kept
            else:
                del hooks[event]
    if "hooks" in settings and not settings["hooks"]:
        del settings["hooks"]
    return changed


def check_key(url, key):
    req = urllib.request.Request(
        url + "/v1/check",
        data=b'{"run_id": "connect-check", "source": "connect"}',
        headers={"Authorization": "Bearer " + key, "content-type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            return r.status == 200, ""
    except urllib.error.HTTPError as e:
        return False, "TokenLens rejected the key (HTTP %d)" % e.code
    except OSError as e:
        return False, "cannot reach TokenLens at %s (%s)" % (url, e)


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--key", help="TokenLens key (tl_...) from the dashboard")
    p.add_argument("--url", default="http://127.0.0.1:8787", help="TokenLens URL (the :8787 listener)")
    p.add_argument("--agent", default="claude-code", help="agent name shown in TokenLens")
    p.add_argument("--user", help="optional user name or email to attribute spend to")
    p.add_argument("--settings", default=os.path.expanduser("~/.claude/settings.json"))
    p.add_argument("--remove", action="store_true", help="undo: remove TokenLens from the settings")
    p.add_argument("--skip-check", action="store_true", help="don't test the key first")
    a = p.parse_args()

    path = a.settings
    settings = {}
    if os.path.exists(path):
        with open(path) as f:
            text = f.read()
        settings = json.loads(text) if text.strip() else {}

    if a.remove:
        if not strip(settings):
            print("TokenLens is not configured in %s; nothing to do." % path)
            return 0
    else:
        if not a.key or not a.key.startswith("tl_"):
            p.error("--key tl_... is required (create one in the TokenLens dashboard)")
        url = a.url.rstrip("/")
        if not a.skip_check:
            ok, why = check_key(url, a.key)
            if not ok:
                print("Not changed: " + why, file=sys.stderr)
                return 1
        env = settings.get("env", {})
        logs_endpoint = env.get("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "")
        foreign = logs_endpoint and "/v1/otel/v1/logs" not in logs_endpoint
        if not logs_endpoint and "otlp" in env.get("OTEL_LOGS_EXPORTER", ""):
            foreign = bool(env.get("OTEL_EXPORTER_OTLP_ENDPOINT"))
        if foreign:
            print(
                "Not changed: Claude Code already sends its logs to another OpenTelemetry collector "
                "(see the OTEL_* settings in %s). It exports logs to one place; remove those "
                "settings first if TokenLens should receive them." % path,
                file=sys.stderr,
            )
            return 1
        strip(settings)
        headers = "Authorization=Bearer %s,x-tl-agent=%s" % (a.key, a.agent)
        if a.user:
            headers += ",x-tl-user=" + a.user
        settings.setdefault("env", {}).update(
            {
                "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
                "OTEL_LOGS_EXPORTER": "otlp",
                "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL": "http/json",
                "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": url + "/v1/otel/v1/logs",
                "OTEL_EXPORTER_OTLP_LOGS_HEADERS": headers,
                "OTEL_LOGS_EXPORT_INTERVAL": "2000",
                "TOKENLENS_API_KEY": a.key,
            }
        )
        hook_headers = {"Authorization": "Bearer $TOKENLENS_API_KEY", "x-tl-agent": a.agent}
        if a.user:
            hook_headers["x-tl-user"] = a.user
        hook = {
            "type": "http",
            "url": url + HOOK_PATH,
            "timeout": 5,
            "headers": hook_headers,
            "allowedEnvVars": ["TOKENLENS_API_KEY"],
        }
        hooks = settings.setdefault("hooks", {})
        for event in HOOK_EVENTS:
            group = {"hooks": [hook]}
            if event == "PreToolUse":
                group = {"matcher": "*", "hooks": [hook]}
            hooks.setdefault(event, []).append(group)

    os.makedirs(os.path.dirname(path), exist_ok=True)
    if os.path.exists(path):
        backup = "%s.bak-tokenlens-%s" % (path, time.strftime("%Y%m%d-%H%M%S"))
        n = 1
        while os.path.exists(backup):
            backup = "%s.bak-tokenlens-%s-%d" % (path, time.strftime("%Y%m%d-%H%M%S"), n)
            n += 1
        shutil.copy2(path, backup)
        print("Backup: " + backup)
    tmp = path + ".tmp-tokenlens"
    with open(tmp, "w") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")
    os.chmod(tmp, 0o600)
    os.replace(tmp, path)
    if a.remove:
        print("Removed TokenLens from %s. New Claude Code sessions won't report to it." % path)
    else:
        print("Connected. New Claude Code sessions report to TokenLens at %s." % a.url)
        print("Start a new session (or restart Claude Code) and send a message; it shows up as run cc-<session id>.")
        print("Undo: run this script again with --remove (or restore the backup above).")
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Connect Gemini CLI on this machine to TokenLens (or disconnect it).

Turns on Gemini CLI's built-in OpenTelemetry export in ~/.gemini/settings.json, so every session
reports its model calls (model, tokens) to TokenLens, and adds command hooks that stop the next
prompt or tool call when an admin pauses the run or an enforced budget is used up. The hooks run
~/.tokenlens/hook.py, which this script writes. The settings file is backed up first.
Standard library only.

    python3 gemini.py --key tl_... --url http://127.0.0.1:8787
    python3 gemini.py --remove
"""

# @tokenlens-common@

TOOL = "gemini"
HOOK_NAME = "tokenlens-budget"
HOOK_EVENTS = ["BeforeAgent", "BeforeTool", "SessionEnd"]
OUR_TELEMETRY = ["enabled", "target", "otlpEndpoint", "otlpProtocol", "otlpHeaders", "logPrompts"]
OTEL_PATH = "/v1/otel"


def is_our_group(group):
    return isinstance(group, dict) and any(
        h.get("name") == HOOK_NAME or is_our_command(h.get("command")) for h in group.get("hooks", [])
    )


def is_ours(telemetry):
    return str(telemetry.get("otlpEndpoint", "")).rstrip("/").endswith(OTEL_PATH)


def strip(settings):
    """Remove everything this script adds. Returns True if anything was removed."""
    changed = False
    telemetry = settings.get("telemetry")
    if isinstance(telemetry, dict) and is_ours(telemetry):
        for k in OUR_TELEMETRY:
            telemetry.pop(k, None)
        if not telemetry:
            del settings["telemetry"]
        changed = True
    hooks = settings.get("hooks")
    if isinstance(hooks, dict) and strip_hook_groups(hooks, is_our_group):
        if not hooks:
            del settings["hooks"]
        changed = True
    return changed


def main():
    p = base_args(__doc__.split("\n\n")[0], "Gemini CLI", "gemini-cli")
    p.add_argument("--settings", default=os.path.join(os.path.expanduser("~"), ".gemini", "settings.json"))
    p.add_argument(
        "--every-model-call",
        action="store_true",
        help="also check before every model call (BeforeModel), not only each prompt and tool call",
    )
    a = p.parse_args()
    path = a.settings
    settings = read_json(path, "Gemini CLI settings")

    if a.remove:
        changed = strip(settings)
        changed = remove_hook(TOOL) or changed
        if not changed:
            print("TokenLens is not configured in %s; nothing to do." % path)
            return 0
        write_json(path, settings)
        print("Removed TokenLens from %s. New Gemini CLI sessions won't report to it." % path)
        return 0

    url = validate(p, a)
    telemetry = settings.get("telemetry") if isinstance(settings.get("telemetry"), dict) else {}
    foreign = not is_ours(telemetry) and (
        telemetry.get("otlpEndpoint") or telemetry.get("outfile") or telemetry.get("target") == "gcp"
    )
    if foreign and telemetry.get("enabled", True):
        print(
            "Not changed: Gemini CLI already sends its telemetry elsewhere (see \"telemetry\" in %s). "
            "It exports to one place; remove that setting first if TokenLens should receive it." % path,
            file=sys.stderr,
        )
        return 1
    strip(settings)
    headers = {"Authorization": "Bearer " + a.key, "x-tl-agent": a.agent}
    if a.user:
        headers["x-tl-user"] = a.user
    telemetry = settings.setdefault("telemetry", {})
    telemetry.update(
        {
            "enabled": True,
            "target": "local",
            "otlpEndpoint": url + OTEL_PATH,
            "otlpProtocol": "http",
            "otlpHeaders": headers,
            "logPrompts": False,
        }
    )
    install_hook(TOOL, url, a.key, a.agent, a.user)
    hooks = settings.setdefault("hooks", {})
    events = HOOK_EVENTS + (["BeforeModel"] if a.every_model_call else [])
    for event in events:
        handler = {
            "name": HOOK_NAME,
            "type": "command",
            "command": hook_command(TOOL),
            "timeout": 5000,
            "description": "TokenLens: stop when the run is paused or a budget is used up",
        }
        group = {"hooks": [handler]}
        if event == "BeforeTool":
            group = {"matcher": "*", "hooks": [handler]}
        hooks.setdefault(event, []).append(group)
    write_json(path, settings)

    print("Connected. New Gemini CLI sessions report to TokenLens at %s." % url)
    print("Start a new session (or restart Gemini CLI); it shows up as run gemini-<session id>.")
    print("Undo: run this script again with --remove (or restore the backup above).")
    return 0


if __name__ == "__main__":
    sys.exit(main())

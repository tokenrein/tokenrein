#!/usr/bin/env python3
"""Connect OpenAI Codex CLI on this machine to Tokenrein (or disconnect it).

Turns on Codex's built-in OpenTelemetry log export in ~/.codex/config.toml, so every session
reports its model calls (model, tokens) to Tokenrein, and adds command hooks to
~/.codex/hooks.json that stop the next prompt or tool call when an admin pauses the run or an
enforced budget is used up. The hooks run ~/.tokenrein/hook.py, which this script writes.
Both Codex files are backed up first. Standard library only.

    python3 codex.py --key tr_... --url http://127.0.0.1:8787
    python3 codex.py --remove
"""

# @tokenrein-common@

TOOL = "codex"
HOOK_EVENTS = ["UserPromptSubmit", "PreToolUse", "SessionEnd"]
BEGIN = "# >>> Tokenrein (added by the Tokenrein connect script; undo with --remove)"
END = "# <<< Tokenrein"
OTEL_TABLE = re.compile(r"^\s*(\[\[?\s*otel\s*[\].]|otel\s*[.=])", re.M)


def codex_home():
    return os.environ.get("CODEX_HOME") or os.path.join(os.path.expanduser("~"), ".codex")


def without_block(text):
    """The config with our [otel] block taken out."""
    out, inside = [], False
    for line in text.splitlines(keepends=True):
        if line.strip() == BEGIN:
            inside = True
        elif inside and line.strip() == END:
            inside = False
        elif not inside:
            out.append(line)
    return "".join(out)


def toml_str(s):
    # JSON string escapes are valid in TOML basic strings.
    return json.dumps(s)


def otel_block(url, key, agent, user):
    headers = [("Authorization", "Bearer " + key), ("x-tr-agent", agent)]
    if user:
        headers.append(("x-tr-user", user))
    h = ", ".join("%s = %s" % (toml_str(k), toml_str(v)) for k, v in headers)
    return "\n".join(
        [
            BEGIN,
            "[otel]",
            'environment = "tokenrein"',
            "log_user_prompt = false",
            "exporter = { otlp-http = { endpoint = %s, protocol = \"json\", headers = { %s } } }"
            % (toml_str(url + "/v1/otel/v1/logs"), h),
            END,
            "",
        ]
    )


def is_our_group(group):
    return isinstance(group, dict) and any(is_our_command(h.get("command")) for h in group.get("hooks", []))


def main():
    p = base_args(__doc__.split("\n\n")[0], "Codex", "codex")
    a = p.parse_args()
    home = codex_home()
    config_path = os.path.join(home, "config.toml")
    hooks_path = os.path.join(home, "hooks.json")
    config = read_text(config_path) or ""
    hooks_doc = read_json(hooks_path, "Codex hooks")

    if a.remove:
        changed = False
        stripped = without_block(config)
        if stripped != config:
            stripped = stripped.rstrip()
            write_text(config_path, stripped + "\n" if stripped else "")
            changed = True
        if strip_hook_groups(hooks_doc.get("hooks", {}), is_our_group):
            if not hooks_doc["hooks"]:
                del hooks_doc["hooks"]
            write_json(hooks_path, hooks_doc)
            changed = True
        changed = remove_hook(TOOL) or changed
        if not changed:
            print("Tokenrein is not configured for Codex in %s; nothing to do." % home)
        else:
            print("Removed Tokenrein from Codex. New Codex sessions won't report to it.")
        return 0

    url = validate(p, a)
    base = without_block(config)
    if OTEL_TABLE.search(base):
        print(
            "Not changed: %s already has an [otel] section (another OpenTelemetry collector). "
            "Codex exports logs to one place; remove that section first if Tokenrein should "
            "receive them." % config_path,
            file=sys.stderr,
        )
        return 1
    base = base.rstrip()
    install_hook(TOOL, url, a.key, a.agent, a.user)
    write_text(config_path, (base + "\n\n" if base else "") + otel_block(url, a.key, a.agent, a.user))

    hooks = hooks_doc.setdefault("hooks", {})
    strip_hook_groups(hooks, is_our_group)
    for event in HOOK_EVENTS:
        handler = {"type": "command", "command": hook_command(TOOL), "timeout": 3 if event == "SessionEnd" else 10}
        group = {"hooks": [handler]}
        if event == "PreToolUse":
            group = {"matcher": "*", "hooks": [handler]}
        hooks.setdefault(event, []).append(group)
    write_json(hooks_path, hooks_doc)

    print("Connected. New Codex sessions report to Tokenrein at %s." % url)
    print("Start Codex and run /hooks once to review and trust the Tokenrein hooks (Codex skips new hooks until you do).")
    print("Each session shows up as run codex-<session id>.")
    print("Undo: run this script again with --remove (or restore the backups above).")
    return 0


if __name__ == "__main__":
    sys.exit(main())

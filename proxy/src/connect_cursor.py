#!/usr/bin/env python3
"""Connect Cursor on this machine to Tokenrein (or disconnect it).

Adds user-level Cursor hooks to ~/.cursor/hooks.json that report each agent conversation to
Tokenrein and stop the next prompt, tool call or sub-agent when an admin pauses the run or an
enforced budget is used up. The hooks run ~/.tokenrein/hook.py, which this script writes. The
hooks file is backed up first. Cursor has no local usage telemetry: a team admin can import
usage from Cursor's Admin API on the Tokenrein Connect page. Standard library only.

    python3 cursor.py --key tr_... --url http://127.0.0.1:8787
    python3 cursor.py --remove
"""

# @tokenrein-common@

TOOL = "cursor"
HOOK_EVENTS = ["beforeSubmitPrompt", "preToolUse", "subagentStart", "sessionEnd"]


def is_our_entry(entry):
    return isinstance(entry, dict) and is_our_command(entry.get("command"))


def main():
    p = base_args(__doc__.split("\n\n")[0], "Cursor", "cursor")
    p.add_argument("--hooks-file", default=os.path.join(os.path.expanduser("~"), ".cursor", "hooks.json"))
    a = p.parse_args()
    path = a.hooks_file
    doc = read_json(path, "Cursor hooks")
    hooks = doc.get("hooks") if isinstance(doc.get("hooks"), dict) else {}

    if a.remove:
        changed = strip_hook_groups(hooks, is_our_entry)
        if changed:
            write_json(path, doc)
        changed = remove_hook(TOOL) or changed
        if not changed:
            print("Tokenrein is not configured in %s; nothing to do." % path)
        else:
            print("Removed Tokenrein from %s." % path)
        return 0

    url = validate(p, a)
    install_hook(TOOL, url, a.key, a.agent, a.user)
    doc.setdefault("version", 1)
    hooks = doc.setdefault("hooks", {})
    strip_hook_groups(hooks, is_our_entry)
    for event in HOOK_EVENTS:
        hooks.setdefault(event, []).append({"command": hook_command(TOOL), "timeout": 5})
    write_json(path, doc)

    print("Connected. Cursor agent conversations report to Tokenrein at %s." % url)
    print("Cursor reloads hooks.json on save; each conversation shows up as run cursor-<conversation id>.")
    print("Undo: run this script again with --remove (or restore the backup above).")
    return 0


if __name__ == "__main__":
    sys.exit(main())

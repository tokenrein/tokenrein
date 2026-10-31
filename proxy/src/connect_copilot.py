#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Connect GitHub Copilot CLI on this machine to Tokenrein (or disconnect it).

Adds user-level Copilot hooks in ~/.copilot/hooks/tokenrein.json that stop the next tool call
when an admin pauses the run or an enforced budget is used up, and turns on Copilot's
OpenTelemetry export (model and tokens of every call) for the `copilot` command through
~/.tokenrein/copilot.sh, sourced from your shell profile. The hooks run ~/.tokenrein/hook.py,
which this script writes. Files of yours that it changes are backed up first. Standard library
only.

With --policy (run as root) it sets this up for every user of the machine instead: Copilot
policy hooks in /etc/github-copilot/policy.d/tokenrein.json, which users can't turn off, and
OpenTelemetry export with each user's GitHub login in Copilot's managed settings.

    python3 copilot.py --key tr_... --url http://127.0.0.1:8787
    sudo python3 copilot.py --policy --key tr_... --url https://tokenrein.example.com
    python3 copilot.py --remove            (sudo python3 copilot.py --policy --remove)
"""

# @tokenrein-common@

TOOL = "copilot"
# Native (camelCase) events. Their payloads don't name the event, so the hook command passes it.
HOOK_EVENTS = ["preToolUse", "userPromptSubmitted", "sessionEnd"]
OTEL_PATH = "/v1/otel"
PROFILE_MARK = "# Tokenrein: GitHub Copilot telemetry (undo: copilot.py --remove)"


def copilot_home():
    return os.environ.get("COPILOT_HOME") or os.path.join(os.path.expanduser("~"), ".copilot")


def hooks_doc(command):
    """Copilot's hooks file: one command hook per event, with the event name as an argument."""
    hooks = {}
    for event in HOOK_EVENTS:
        timeout = 3 if event == "sessionEnd" else 10
        hooks[event] = [{"type": "command", "bash": "%s %s" % (command, event), "timeoutSec": timeout}]
    return {"version": 1, "hooks": hooks}


def is_ours_file(path):
    """True if the hooks file at `path` runs the Tokenrein hook (from ~/.tokenrein or /etc)."""
    if not os.path.exists(path):
        return False
    doc = read_json(path, "Copilot hooks")
    groups = doc.get("hooks") if isinstance(doc.get("hooks"), dict) else {}
    return any(
        isinstance(e, dict) and "tokenrein/hook.py" in str(e.get("bash", ""))
        for entries in groups.values()
        if isinstance(entries, list)
        for e in entries
    )


def otel_env(url, key, agent, user):
    headers = "Authorization=Bearer %s,x-tr-agent=%s" % (key, agent)
    if user:
        headers += ",x-tr-user=" + user
    env = [
        ("COPILOT_OTEL_ENABLED", "true"),
        ("OTEL_EXPORTER_OTLP_ENDPOINT", url + OTEL_PATH),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
        ("OTEL_EXPORTER_OTLP_HEADERS", headers),
        # Copilot otherwise sends a short session's spans only when it ends, too late for its
        # spend to stop the session's next tool call.
        ("OTEL_BSP_SCHEDULE_DELAY", "1000"),
    ]
    # Only the copilot command gets these, so other OpenTelemetry programs keep their own setup.
    assigns = " ".join("%s=%s" % (k, shlex.quote(v)) for k, v in env)
    return "\n".join(
        [
            "# Written by the Tokenrein connect script: GitHub Copilot CLI reports its model calls",
            "# (model and tokens, no prompts) to Tokenrein. Undo: copilot.py --remove",
            "copilot() {",
            '  %s command copilot "$@"' % assigns,
            "}",
            "",
        ]
    )


def profiles():
    """Shell profiles to source copilot.sh from: the existing ones, else the login shell's."""
    home = os.path.expanduser("~")
    found = [os.path.join(home, f) for f in (".zshrc", ".bashrc") if os.path.exists(os.path.join(home, f))]
    if found:
        return found
    shell = os.path.basename(os.environ.get("SHELL", ""))
    return [os.path.join(home, ".zshrc" if shell == "zsh" else ".bashrc")]


def profile_line(env_path):
    q = shlex.quote(env_path)
    return "[ -f %s ] && . %s  %s\n" % (q, q, PROFILE_MARK)


def strip_profiles():
    """Take our line out of the shell profiles. Returns True if any changed."""
    changed = False
    for name in (".zshrc", ".bashrc"):
        path = os.path.join(os.path.expanduser("~"), name)
        text = read_text(path)
        if text is None or PROFILE_MARK not in text:
            continue
        kept = "".join(line for line in text.splitlines(keepends=True) if PROFILE_MARK not in line)
        write_text(path, kept, mode=os.stat(path).st_mode & 0o777)
        changed = True
    return changed


def write_own(path, text, mode=0o600):
    """Write a file that only Tokenrein uses. No backup: it holds the key, and nothing of the
    user's is lost by replacing it."""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp-tokenrein"
    with open(tmp, "w") as f:
        f.write(text)
    os.chmod(tmp, mode)
    os.replace(tmp, path)


def remove_file(path):
    """Delete a file of Tokenrein's, and its directory if that is left empty."""
    if not os.path.exists(path):
        return False
    os.remove(path)
    try:
        os.rmdir(os.path.dirname(path))
    except OSError:
        pass
    return True


# --- Every user of the machine (--policy) ---


def policy_paths(root):
    """Copilot's policy hooks file, its managed settings file, and Tokenrein's directory."""
    if sys.platform == "darwin":
        managed = os.path.join(root, "Library", "Application Support", "GitHubCopilot", "managed-settings.json")
    else:
        managed = os.path.join(root, "etc", "github-copilot", "managed-settings.json")
    hooks = os.path.join(root, "etc", "github-copilot", "policy.d", "tokenrein.json")
    return hooks, managed, os.path.join(root, "etc", "tokenrein")


def is_our_telemetry(t):
    return (
        isinstance(t, dict)
        and str(t.get("endpoint", "")).rstrip("/").endswith(OTEL_PATH)
        and isinstance(t.get("headers"), dict)
        and "x-tr-agent" in t["headers"]
    )


def write_managed(path, doc):
    # Copilot ignores managed settings that aren't owned by root or are world-writable.
    write_text(path, json.dumps(doc, indent=2) + "\n", mode=0o644)


def policy(p, a):
    root = a.policy_root or "/"
    if not a.policy_root and hasattr(os, "geteuid") and os.geteuid() != 0:
        p.error("--policy writes machine-wide files: run it as root (sudo python3 copilot.py --policy ...)")
    hooks_path, managed_path, d = policy_paths(root)
    managed = read_json(managed_path, "Copilot managed settings")

    if a.remove:
        changed = remove_file(hooks_path) if is_ours_file(hooks_path) else False
        if is_our_telemetry(managed.get("telemetry")):
            del managed["telemetry"]
            write_managed(managed_path, managed)
            changed = True
        changed = remove_hook(TOOL, d) or changed
        if not changed:
            print("The Tokenrein policy setup for Copilot is not on this machine; nothing to do.")
        else:
            print("Removed the Tokenrein policy setup for Copilot. New sessions won't report to it.")
        return 0

    url = validate(p, a)
    telemetry = managed.get("telemetry")
    if telemetry and not is_our_telemetry(telemetry):
        print(
            "Not changed: %s already sends Copilot's telemetry elsewhere (\"telemetry\"). Copilot "
            "exports to one place; remove that setting first if Tokenrein should receive it." % managed_path,
            file=sys.stderr,
        )
        return 1
    # Readable by every user: hooks run as the user, and read the key from here.
    install_hook(TOOL, url, a.key, a.agent, a.user, d=d, mode=0o644)
    command = "%s %s %s" % (shlex.quote(sys.executable or "python3"), shlex.quote(os.path.join(d, "hook.py")), TOOL)
    write_own(hooks_path, json.dumps(hooks_doc(command), indent=2) + "\n", mode=0o644)
    headers = {"Authorization": "Bearer " + a.key, "x-tr-agent": a.agent}
    if a.user:
        headers["x-tr-user"] = a.user
    managed["telemetry"] = {
        "enabled": True,
        "endpoint": url + OTEL_PATH,
        "protocol": "http/json",
        "headers": headers,
        # Each developer's GitHub login as user.name; no prompts, responses or tool content.
        "capture": {"identity": True},
    }
    write_managed(managed_path, managed)

    print("Connected every Copilot CLI user on this machine to Tokenrein at %s." % url)
    print("Policy hooks: %s (users can't turn them off)." % hooks_path)
    print("Telemetry: %s (VS Code reads it too)." % managed_path)
    print("The key is in %s, readable by every local user." % os.path.join(d, TOOL + ".json"))
    print("Each session shows up as run copilot-<session id>, with the developer's GitHub login.")
    return 0


def main():
    p = base_args(__doc__.split("\n\n")[0], "GitHub Copilot CLI", "copilot")
    p.add_argument("--policy", action="store_true", help="set up every user of this machine (run as root)")
    p.add_argument("--policy-root", help=argparse.SUPPRESS)
    p.add_argument("--no-profile", action="store_true", help="don't edit ~/.zshrc or ~/.bashrc")
    a = p.parse_args()
    if a.policy:
        return policy(p, a)

    hooks_path = os.path.join(copilot_home(), "hooks", "tokenrein.json")
    env_path = os.path.join(tokenrein_dir(), "copilot.sh")

    if a.remove:
        changed = remove_file(hooks_path) if is_ours_file(hooks_path) else False
        changed = strip_profiles() or changed
        if os.path.exists(env_path):
            os.remove(env_path)
            changed = True
        changed = remove_hook(TOOL) or changed
        if not changed:
            print("Tokenrein is not configured for Copilot CLI; nothing to do.")
        else:
            print("Removed Tokenrein from Copilot CLI. Open a new terminal; new sessions won't report to it.")
        return 0

    url = validate(p, a)
    if os.path.exists(hooks_path) and not is_ours_file(hooks_path):
        print("Not changed: %s exists and isn't Tokenrein's. Rename it first." % hooks_path, file=sys.stderr)
        return 1
    install_hook(TOOL, url, a.key, a.agent, a.user)
    write_own(hooks_path, json.dumps(hooks_doc(hook_command(TOOL)), indent=2) + "\n")
    write_own(env_path, otel_env(url, a.key, a.agent, a.user))
    if not a.no_profile:
        for path in profiles():
            text = read_text(path)
            if text is not None and PROFILE_MARK in text:
                continue
            text = text or ""
            sep = "" if not text or text.endswith("\n") else "\n"
            mode = os.stat(path).st_mode & 0o777 if os.path.exists(path) else 0o644
            write_text(path, text + sep + profile_line(env_path), mode=mode)

    print("Connected. Copilot CLI sessions report to Tokenrein at %s." % url)
    print("Hooks: %s" % hooks_path)
    if a.no_profile:
        print("Telemetry: add this line to your shell profile, then open a new terminal:")
        print("  " + profile_line(env_path).strip())
    else:
        print("Telemetry: %s, sourced from your shell profile. Open a new terminal before starting copilot." % env_path)
    print("Each session shows up as run copilot-<session id>.")
    print("Undo: run this script again with --remove (or restore the backups above).")
    return 0


if __name__ == "__main__":
    sys.exit(main())

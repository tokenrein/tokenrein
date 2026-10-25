# --- Shared by the Tokenrein connect scripts (inserted where each script is served) ---

import argparse
import json
import os
import re
import shlex
import shutil
import sys
import time
import urllib.error
import urllib.request

# The hook every command-hook agent runs; see connect_hook.py in the Tokenrein source.
HOOK_PY = r'''@tokenrein-hook@'''
NAME_RE = re.compile(r"^[A-Za-z0-9._@+-]{1,128}$")


def tokenrein_dir():
    return os.path.join(os.path.expanduser("~"), ".tokenrein")


def hook_command(tool):
    """The command the agent runs; absolute paths, because hooks may run with a bare PATH."""
    script = os.path.join(tokenrein_dir(), "hook.py")
    return "%s %s %s" % (shlex.quote(sys.executable or "python3"), shlex.quote(script), tool)


def is_our_command(cmd):
    # Installs made before the rename used ~/.tokenlens (pre-rename name).
    old = "/.tokenlens/hook.py"  # pre-rename name
    return isinstance(cmd, str) and ("/.tokenrein/hook.py" in cmd or old in cmd)


def base_args(description, tool, agent):
    p = argparse.ArgumentParser(description=description)
    p.add_argument("--key", help="Tokenrein key (tr_...) from the dashboard")
    p.add_argument("--url", default="http://127.0.0.1:8787", help="Tokenrein URL (the :8787 listener)")
    p.add_argument("--agent", default=agent, help="agent name shown in Tokenrein")
    p.add_argument("--user", help="optional user name or email to attribute spend to")
    p.add_argument("--remove", action="store_true", help="undo: remove Tokenrein from %s" % tool)
    p.add_argument("--skip-check", action="store_true", help="don't test the key first")
    return p


def validate(p, a):
    """Check --key/--agent/--user and the key itself. Returns the URL without a trailing slash."""
    if not a.key or not a.key.startswith(("tr_", "tl_")) or not re.match(r"^[A-Za-z0-9_-]+$", a.key):
        p.error("--key tr_... is required (create one in the Tokenrein dashboard)")
    if not NAME_RE.match(a.agent):
        p.error("--agent must be 1-128 letters, digits, '.', '_', '-', '@' or '+'")
    if a.user and not NAME_RE.match(a.user):
        p.error("--user must be 1-128 letters, digits, '.', '_', '-', '@' or '+'")
    url = a.url.rstrip("/")
    if not re.match(r"^https?://[^\s\"'\\]+$", url):
        p.error("--url must be an http(s) URL")
    if not a.skip_check:
        ok, why = check_key(url, a.key)
        if not ok:
            print("Not changed: " + why, file=sys.stderr)
            sys.exit(1)
    return url


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
        return False, "Tokenrein rejected the key (HTTP %d)" % e.code
    except OSError as e:
        return False, "cannot reach Tokenrein at %s (%s)" % (url, e)


def read_text(path):
    if not os.path.exists(path):
        return None
    with open(path) as f:
        return f.read()


def read_json(path, what):
    text = read_text(path)
    if text is None or not text.strip():
        return {}
    try:
        doc = json.loads(text)
    except ValueError as e:
        print("Not changed: %s is not plain JSON (%s). Edit it by hand instead." % (path, e), file=sys.stderr)
        sys.exit(1)
    if not isinstance(doc, dict):
        print("Not changed: %s should hold a JSON object (%s)." % (path, what), file=sys.stderr)
        sys.exit(1)
    return doc


def backup(path):
    """Copy the file next to itself before changing it; returns the copy's path."""
    if not os.path.exists(path):
        return None
    stamp = time.strftime("%Y%m%d-%H%M%S")
    copy = "%s.bak-tokenrein-%s" % (path, stamp)
    n = 1
    while os.path.exists(copy):
        copy = "%s.bak-tokenrein-%s-%d" % (path, stamp, n)
        n += 1
    shutil.copy2(path, copy)
    print("Backup: " + copy)
    return copy


def write_text(path, text, mode=0o600):
    """Replace the file atomically, after saving a backup of it."""
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    backup(path)
    tmp = path + ".tmp-tokenrein"
    with open(tmp, "w") as f:
        f.write(text)
    os.chmod(tmp, mode)
    os.replace(tmp, path)


def write_json(path, doc):
    write_text(path, json.dumps(doc, indent=2) + "\n")


def install_hook(tool, url, key, agent, user):
    """Write ~/.tokenrein/hook.py and the tool's settings for it (URL and key, owner-only)."""
    d = tokenrein_dir()
    os.makedirs(d, mode=0o700, exist_ok=True)
    with open(os.path.join(d, "hook.py"), "w") as f:
        f.write(HOOK_PY)
    cfg = {"url": url, "key": key, "agent": agent, "user": user or None, "timeout": 4}
    path = os.path.join(d, tool + ".json")
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(cfg, f, indent=2)
        f.write("\n")
    os.chmod(tmp, 0o600)
    os.replace(tmp, path)


def remove_hook(tool):
    """Delete the tool's hook settings, and the hook once no tool uses it."""
    d = tokenrein_dir()
    removed = False
    path = os.path.join(d, tool + ".json")
    if os.path.exists(path):
        os.remove(path)
        removed = True
    others = [t for t in ("codex", "gemini", "cursor") if os.path.exists(os.path.join(d, t + ".json"))]
    if not others and os.path.exists(os.path.join(d, "hook.py")):
        os.remove(os.path.join(d, "hook.py"))
    return removed


def strip_hook_groups(hooks, matches):
    """Remove our entries from a {event: [group, ...]} map. Returns True if anything changed."""
    changed = False
    for event in list(hooks):
        groups = hooks[event] if isinstance(hooks[event], list) else []
        kept = [g for g in groups if not matches(g)]
        if len(kept) != len(groups):
            changed = True
            if kept:
                hooks[event] = kept
            else:
                del hooks[event]
    return changed

# --- End of shared code ---

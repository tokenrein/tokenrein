#!/usr/bin/env python3
"""Four agents that misbehave in different ways, calling a fake model through Tokenrein.

Set up by demo/run.sh. Only the standard library: each agent is a plain HTTP client whose base URL
points at Tokenrein, so what is printed here is what Tokenrein really answered.
"""
import json
import os
import sys
import threading
import time
import urllib.error
import urllib.request

PROXY = os.environ.get("DEMO_PROXY", "http://127.0.0.1:19787")
KEYS = json.loads(os.environ["DEMO_KEYS"])

RESET, DIM, BOLD = "\033[0m", "\033[2m", "\033[1m"
RED, GREEN, YELLOW, CYAN, MAGENTA = "\033[31m", "\033[32m", "\033[33m", "\033[36m", "\033[35m"
USERS = {
    "nightly-summary": "data-team@example.com",
    "support-bot": "ana@example.com",
    "research-agent": "sam@example.com",
    "batch-reindex": "platform@example.com",
}
lock = threading.Lock()


def say(name, color, text):
    with lock:
        print(f"{color}{BOLD}{name:<15}{RESET} {text}", flush=True)


def call(project, agent, run, prompt):
    """One model call through Tokenrein: (HTTP status, x-tr-warning, error type, cost in USD)."""
    body = json.dumps(
        {"model": "gpt-4o", "max_tokens": 2000, "messages": [{"role": "user", "content": prompt}]}
    ).encode()
    req = urllib.request.Request(
        f"{PROXY}/v1/chat/completions",
        data=body,
        headers={
            "content-type": "application/json",
            "authorization": f"Bearer {KEYS[project]}",
            "x-tr-agent": agent,
            "x-tr-run-id": run,
            "x-tr-user": USERS[agent],
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as res:
            usage = json.load(res)["usage"]
            cost = (usage["prompt_tokens"] * 2.5 + usage["completion_tokens"] * 10) / 1e6
            return 200, res.headers.get("x-tr-warning"), None, cost
    except urllib.error.HTTPError as err:
        kind = json.load(err).get("error", {}).get("type", "")
        return err.code, None, kind, 0.0


def warns_only():
    """A budget in Check mode: the warning comes back on the response and the agent carries on."""
    name, spent, warned = "nightly-summary", 0.0, False
    for step in range(1, 11):
        status, warning, _, cost = call("data-pipeline", name, "summary-1101", f"summarise batch {step}")
        spent += cost
        if warning and not warned:
            warned = True
            say(name, YELLOW, f"{YELLOW}{BOLD}⚠ TOKEN WARNING{RESET} {warning}")
            say(name, YELLOW, f"{DIM}Check mode: logged, nothing blocked. The agent carries on.{RESET}")
        elif step % 3 == 0:
            say(name, YELLOW, f"call {step:<2} ok   {DIM}spent ${spent:.2f}{RESET}")
    say(name, YELLOW, f"{GREEN}✓ finished{RESET} {DIM}${spent:.2f}{RESET}")


def wait_for_decision(project, agent, run):
    """Ask Tokenrein until a person has decided; True if the run may continue."""
    body = json.dumps({"run_id": run, "agent": agent, "model": "gpt-4o", "max_output_tokens": 2000}).encode()
    while True:
        req = urllib.request.Request(
            f"{PROXY}/v1/check",
            data=body,
            headers={"content-type": "application/json", "authorization": f"Bearer {KEYS[project]}"},
        )
        try:
            with urllib.request.urlopen(req, timeout=30) as res:
                answer = json.load(res)
        except urllib.error.HTTPError as err:
            answer = json.load(err)
        if answer.get("allowed"):
            return True
        if answer.get("run_state") == "killed":
            return False
        time.sleep(0.5)


def needs_approval():
    """A run budget in Ask mode: the run waits for a person, then continues where it stopped."""
    name, run, spent, step = "support-bot", "ticket-4812", 0.0, 0
    while step < 22:
        status, _, kind, cost = call("support", name, run, f"draft reply, step {step}")
        if status == 200:
            step += 1
            spent += cost
            if step % 4 == 0:
                say(name, CYAN, f"call {step:<2} ok   {DIM}spent ${spent:.2f}{RESET}")
        elif kind == "tokenrein_run_paused":
            say(name, CYAN, f"{RED}{BOLD}⏸ PAUSED{RESET} over its $1.00 budget at ${spent:.2f}, asking an approver")
            if not wait_for_decision("support", name, run):
                say(name, CYAN, f"{RED}✗ stopped by an approver{RESET}")
                return
            say(name, CYAN, f"{GREEN}▶ approved, the run continues{RESET}")
        else:
            say(name, CYAN, f"{RED}✗ stopped: {kind}{RESET}")
            return
    say(name, CYAN, f"{GREEN}✓ finished{RESET} {DIM}${spent:.2f}, ticket answered{RESET}")


def stuck_in_a_loop():
    """The same request again and again: the loop guard pauses the run; a person stops it."""
    name, run, spent, step = "research-agent", "research-77", 0.0, 0
    time.sleep(1.2)
    say(name, MAGENTA, f"{DIM}retrying the same tool call…{RESET}")
    while True:
        status, _, kind, cost = call("research", name, run, "search the web for the same thing")
        if status == 200:
            step += 1
            spent += cost
            say(name, MAGENTA, f"call {step:<2} same request again   {DIM}spent ${spent:.2f}{RESET}")
            time.sleep(0.25)
        elif kind == "tokenrein_run_paused":
            say(name, MAGENTA, f"{RED}{BOLD}⏸ LOOP DETECTED{RESET} same request over and over, run paused")
            if not wait_for_decision("research", name, run):
                say(name, MAGENTA, f"{RED}{BOLD}✗ KILLED{RESET} by an approver at ${spent:.2f}. The loop is over.")
                return
        else:
            say(name, MAGENTA, f"{RED}✗ stopped: {kind}{RESET}")
            return


def hard_limit():
    """A project budget in Enforce mode: the call that would go over is refused."""
    name, spent = "batch-reindex", 0.0
    for step in range(1, 40):
        status, _, kind, cost = call("batch-jobs", name, "reindex-1101", f"reindex shard {step}")
        if status != 200:
            say(name, RED, f"{RED}{BOLD}✗ BLOCKED{RESET} HTTP {status} {kind}")
            say(name, RED, f"{DIM}Refused before it reached the model. Spend stopped at ${spent:.2f} of a $0.50 limit.{RESET}")
            return
        spent += cost
        if step % 2 == 0:
            say(name, RED, f"call {step:<2} ok   {DIM}spent ${spent:.2f} of $0.50{RESET}")
        time.sleep(0.3)


def main():
    warns_only()
    print(flush=True)
    time.sleep(0.8)
    threads = [threading.Thread(target=needs_approval), threading.Thread(target=stuck_in_a_loop)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    print(flush=True)
    time.sleep(float(os.environ.get("DEMO_PAUSE", 0.8)))
    hard_limit()


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)

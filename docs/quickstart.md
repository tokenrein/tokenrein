# Quick start: from zero to a guarded agent

About 15 minutes on one machine with Docker (or Podman) and `openssl`. The image is public
(`ghcr.io/tokenrein/tokenrein`, amd64 and arm64); the free tier covers 25 developers and $10,000
of AI spend a month.

## 1. Deploy

```bash
git clone https://github.com/tokenrein/tokenrein && cd tokenrein/deploy
./setup.sh          # writes .env with fresh secrets; for production: ./setup.sh https://tokenrein.example.com
docker compose up -d
docker compose ps   # tokenrein, redis and postgres "healthy" within about 30 seconds
```

- Back up `TOKENREIN_SECRETS_KEY` from `.env`: it encrypts the provider keys you save later.
- Ports 8787 or 8788 taken: set `TOKENREIN_PROXY_PORT` / `TOKENREIN_ADMIN_PORT` in `.env`, and
  `TOKENREIN_PROXY_URL=http://<host>:<port>` so the dashboard's connect commands use it.
- The dashboard listens on `127.0.0.1` only; for a team, put it behind your HTTPS proxy or VPN.
  More in [install.md](install.md).

## 2. First sign-in

```bash
docker compose exec tokenrein tokenrein create-owner you@company.com
```

Open the printed link (valid 72 hours) and choose a password of 12 or more characters. Then
open **Keys & limits** and create your org under **New org**; the Overview's **Get started**
checklist appears once the org exists.

## 3. A project, a key and a budget

On **Keys & limits**: create a project (spend is counted and limited per project), create a key
(the `tr_…` value is shown once), and set limits:

| Mode | What it does |
| --- | --- |
| Check | Lets calls through and records what Enforce would have blocked. Start here. |
| Enforce | Blocks the call that would go over. |
| Ask | Run budgets only: pauses the run until an approver adds budget. |
| Skip | Only tracks spend. |

The loop guard (same project card) warns, pauses or kills a run that repeats the same request or
tool call, spends too fast, or makes too many calls. A provider key is only needed when agents
call through the Tokenrein proxy.

## 4. Connect a coding agent

**Connect** → pick the agent → choose the project → **Create key & show command**, then run the
command on each developer's machine, for example:

```bash
curl -fsSL http://<dashboard>/connect/claude-code.py | python3 - --key tr_… --url http://<host>:8787
```

New sessions show up under **Runs** (Claude Code as `cc-<session>`). Codex CLI, Gemini CLI and
Cursor work the same way; what each connector changes is in [connectors.md](connectors.md).
Undo with `--remove`.

## 5. Your own app

```bash
pip install "tokenrein[openai]"          # or: npm install @tokenrein/sdk openai
export TOKENREIN_API_KEY=tr_…
export TOKENREIN_BASE_URL=http://<host>:8787
```

```python
from openai import OpenAI
from tokenrein import guard, BudgetExceeded

client = guard.openai(OpenAI(), agent="support-bot")
with guard.run("ticket-4812", budget_usd=5):
    client.chat.completions.create(model="gpt-4o-mini", messages=[{"role": "user", "content": "hi"}])
```

Calls go straight to the provider; Tokenrein gets token counts, never prompts. See the
[SDK guide](../sdk/README.md).

## 6. See it work

- Set the project's monthly limit to a few cents in **Enforce**: the agent's next prompt or tool
  call is refused with the budget it hit.
- Set the run budget in **Ask**: an over-budget run pauses and appears under **Approvals**;
  **Approve** adds budget and the run continues.
- Turn on the loop guard with *pause after 3*: the third identical tool call pauses the run.

Then roll out: a week in Check, an alert channel, Enforce for the limits you trust. Licensing
and paid features: [licensing.md](licensing.md).

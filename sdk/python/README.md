# tokenrein (Python)

Budgets, run tracing and spend reports for AI agents, in one line around your existing OpenAI
or Anthropic client. By default (direct mode) the client keeps calling its provider and only
token usage goes to [Tokenrein](../README.md); `mode="proxy"` sends calls through the Tokenrein
proxy instead. Errors: `BudgetExceeded`, `RunPaused`, `RunKilled`. Works with any
OpenAI-compatible provider (Gemini, Groq, Mistral, DeepSeek, OpenRouter, Ollama, ...).

```bash
pip install "tokenrein[openai]"   # or tokenrein[anthropic]
```

```python
from openai import OpenAI
from tokenrein import guard, BudgetExceeded

client = guard.openai(OpenAI(), agent="support-bot")   # TOKENREIN_API_KEY, TOKENREIN_BASE_URL
with guard.run("ticket-4812", budget_usd=5):
    with guard.run(budget_usd=1):                       # sub-agent: parent set automatically
        client.chat.completions.create(model="gpt-4o-mini", messages=[...])
```

See [../README.md](../README.md) for the full API.

## Develop

```bash
python3 -m venv .venv && .venv/bin/pip install -e ".[test]"
.venv/bin/pytest
```

License: Apache-2.0.

# tokenlens (Python)

Budgets, run tracing and spend reports for AI agents, in one line around your existing OpenAI
or Anthropic client. By default (direct mode) the client keeps calling its provider and only
token usage goes to [TokenLens](../README.md); `mode="proxy"` sends calls through the TokenLens
proxy instead. Errors: `BudgetExceeded`, `RunPaused`, `RunKilled`. Works with any
OpenAI-compatible provider (Gemini, Groq, Mistral, DeepSeek, OpenRouter, Ollama, ...).

```bash
pip install "tokenlens[openai]"   # or tokenlens[anthropic]
```

```python
from openai import OpenAI
from tokenlens import guard, BudgetExceeded

client = guard.openai(OpenAI(), agent="support-bot")   # TOKENLENS_API_KEY, TOKENLENS_BASE_URL
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

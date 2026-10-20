# tokenlens (Python)

Thin client for the [TokenLens](../README.md) proxy. Wraps your existing `openai` or
`anthropic` client so every call carries the run, parent run, agent and run budget headers,
and turns the proxy's 402/403 responses into `BudgetExceeded`, `RunPaused` and `RunKilled`.

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

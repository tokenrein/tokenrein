# @tokenrein/sdk (TypeScript)

Budgets, run tracing and spend reports for AI agents, in one line around your existing OpenAI
or Anthropic client. By default (direct mode) the client keeps calling its provider and only
token usage goes to [Tokenrein](../README.md); `mode="proxy"` sends calls through the Tokenrein
proxy instead. Errors: `BudgetExceeded`, `RunPaused`, `RunKilled`. Works with any
OpenAI-compatible provider (Gemini, Groq, Mistral, DeepSeek, OpenRouter, Ollama, ...).

```bash
npm install @tokenrein/sdk openai   # or @anthropic-ai/sdk
```

```ts
import OpenAI from 'openai';
import { guard, BudgetExceeded } from '@tokenrein/sdk';

const client = guard.openai(new OpenAI(), { agent: 'support-bot' }); // TOKENREIN_API_KEY, TOKENREIN_BASE_URL
await guard.run({ runId: 'ticket-4812', budgetUsd: 5 }, async () => {
  await guard.run({ budgetUsd: 1 }, () =>                            // sub-agent: parent set automatically
    client.chat.completions.create({ model: 'gpt-4o-mini', messages: [{ role: 'user', content: 'hi' }] }),
  );
});
```

Node ≥ 20 (uses `AsyncLocalStorage`). See [../README.md](../README.md) for the full API.

## Develop

```bash
npm install && npm run typecheck && npm test && npm run build
```

License: Apache-2.0.

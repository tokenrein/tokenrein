# @tokenlens/sdk (TypeScript)

Thin client for the [TokenLens](../README.md) proxy. Wraps your existing `openai` or
`@anthropic-ai/sdk` client so every call carries the run, parent run, agent and run budget
headers, and turns the proxy's 402/403 responses into `BudgetExceeded`, `RunPaused` and
`RunKilled`.

```bash
npm install @tokenlens/sdk openai   # or @anthropic-ai/sdk
```

```ts
import OpenAI from 'openai';
import { guard, BudgetExceeded } from '@tokenlens/sdk';

const client = guard.openai(new OpenAI(), { agent: 'support-bot' }); // TOKENLENS_API_KEY, TOKENLENS_BASE_URL
await guard.run({ runId: 'ticket-4812', budgetUsd: 5 }, async () => {
  await guard.run({ budgetUsd: 1 }, () =>                            // sub-agent: parent set automatically
    client.chat.completions.create({ model: 'gpt-4o-mini', messages: [{ role: 'user', content: 'hi' }] }),
  );
});
```

Node ≥ 18 (uses `AsyncLocalStorage`). See [../README.md](../README.md) for the full API.

## Develop

```bash
npm install && npm run typecheck && npm test && npm run build
```

License: Apache-2.0.

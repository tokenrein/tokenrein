import Anthropic from '@anthropic-ai/sdk';
import OpenAI from 'openai';
import { afterAll, beforeAll, beforeEach, describe, expect, it } from 'vitest';

import {
  BudgetExceeded,
  LocalCapExceeded,
  RunKilled,
  RunPaused,
  RunStopped,
  TokenLensError,
  currentRun,
  guard,
} from '../src/index.js';
import { FakeProxy } from './fake-proxy.js';

const proxy = new FakeProxy();
const MSG = [{ role: 'user' as const, content: 'hi' }];

beforeAll(async () => {
  await proxy.start();
});
afterAll(async () => {
  await proxy.close();
});
beforeEach(() => {
  proxy.reset();
  process.env.TOKENLENS_BASE_URL = proxy.url;
  process.env.TOKENLENS_API_KEY = 'tl_test_key';
  // These tests exercise proxy mode; direct mode has its own tests.
  process.env.TOKENLENS_MODE = 'proxy';
});

const openaiClient = (opts: guard.GuardOptions = {}) =>
  guard.openai(new OpenAI({ apiKey: 'sk-real-openai-key' }), opts);
const anthropicClient = (opts: guard.GuardOptions = {}) =>
  guard.anthropic(new Anthropic({ apiKey: 'sk-ant-real' }), opts);
const chat = (c: OpenAI, extra: Partial<OpenAI.ChatCompletionCreateParamsNonStreaming> = {}, headers?: Record<string, string>) =>
  c.chat.completions.create({ model: 'gpt-4o-mini', messages: MSG, ...extra }, headers ? { headers } : undefined);
const message = (c: Anthropic, content = 'hi') =>
  c.messages.create({ model: 'claude-haiku-4-5', max_tokens: 16, messages: [{ role: 'user', content }] });

async function caught(p: Promise<unknown>): Promise<any> {
  try {
    await p;
  } catch (e) {
    return e;
  }
  throw new Error('expected the call to throw');
}

describe('configuration', () => {
  it('points OpenAI at the proxy with the virtual key', async () => {
    const res = await chat(openaiClient({ agent: 'support-bot' }));
    expect(res.choices[0]!.message.content).toBe('ok');
    const { path, headers } = proxy.last();
    expect(path).toBe('/v1/chat/completions');
    expect(headers.authorization).toBe('Bearer tl_test_key');
    expect(headers['x-tl-agent']).toBe('support-bot');
    // Outside a run: no run headers, the proxy makes one run per request.
    expect(headers['x-tl-run-id']).toBeUndefined();
    expect(headers['x-tl-parent-run-id']).toBeUndefined();
  });

  it('points Anthropic at the proxy and drops a provider bearer token', async () => {
    const raw = new Anthropic({ apiKey: 'sk-ant-real', authToken: 'real-oauth-token' });
    const res = await message(guard.anthropic(raw, { agent: 'writer' }));
    expect(res.content[0]).toMatchObject({ type: 'text', text: 'ok' });
    const { path, headers } = proxy.last();
    expect(path).toBe('/v1/messages');
    expect(headers['x-api-key']).toBe('tl_test_key');
    expect(headers.authorization).toBeUndefined();
    expect(headers['x-tl-agent']).toBe('writer');
  });

  it('explicit options override env, and a /v1 base URL is accepted', async () => {
    process.env.TOKENLENS_BASE_URL = 'http://127.0.0.1:1'; // would fail if used
    const c = guard.openai(new OpenAI({ apiKey: 'x' }), { apiKey: 'tl_arg_key', baseURL: `${proxy.url}/v1/` });
    await chat(c);
    expect(proxy.last().path).toBe('/v1/chat/completions');
    expect(proxy.last().headers.authorization).toBe('Bearer tl_arg_key');
  });

  it('fails clearly without a key or with bad ids', () => {
    delete process.env.TOKENLENS_API_KEY;
    expect(() => guard.openai(new OpenAI({ apiKey: 'x' }))).toThrow(/TOKENLENS_API_KEY/);
    expect(() => guard.openai(new OpenAI({ apiKey: 'x' }), { apiKey: 'tl', agent: 'bad/agent' })).toThrow(TypeError);
    expect(() => guard.openai({} as OpenAI, { apiKey: 'tl' })).toThrow(/client instance/);
    expect(() => guard.run('has space', () => 0)).toThrow(TypeError);
    expect(() => guard.run({ budgetUsd: 0 }, () => 0)).toThrow(RangeError);
    expect(() => guard.run({ maxCalls: 1.5 }, () => 0)).toThrow(RangeError);
  });

  it('leaves the original client untouched and keeps the guard through withOptions', async () => {
    const raw = new OpenAI({ apiKey: 'sk-real' });
    const c = guard.openai(raw, { agent: 'bot' });
    expect(raw.baseURL).toMatch(/^https:\/\/api\.openai\.com/);
    expect(c).toBeInstanceOf(OpenAI);
    expect(c.constructor.name).toBe('OpenAI');
    // withOptions exists on openai >= 5; older clients are just reused.
    const copy = typeof c.withOptions === 'function' ? c.withOptions({ timeout: 5000 }) : c;
    await guard.run('r1', () => chat(copy));
    expect(proxy.last().headers['x-tl-agent']).toBe('bot');
    expect(proxy.last().headers['x-tl-run-id']).toBe('r1');
  });
});

describe('run headers', () => {
  it('sets run id and budget, and returns the callback result', async () => {
    const c = openaiClient({ agent: 'bot' });
    const out = await guard.run({ runId: 'ticket-4812', budgetUsd: 5 }, async (run) => {
      expect(currentRun()).toBe(run);
      await chat(c);
      return 42;
    });
    expect(out).toBe(42);
    expect(currentRun()).toBeUndefined();
    const h = proxy.last().headers;
    expect(h['x-tl-run-id']).toBe('ticket-4812');
    expect(Number(h['x-tl-run-budget-usd'])).toBe(5);
    expect(h['x-tl-agent']).toBe('bot');
    expect(h['x-tl-parent-run-id']).toBeUndefined();
  });

  it('generates a valid run id that is stable within the run', async () => {
    const c = openaiClient();
    const id = await guard.run(async (run) => {
      await chat(c);
      await chat(c);
      return run.runId;
    });
    expect(id).toMatch(/^run_[0-9a-f]{32}$/);
    expect(new Set(proxy.requests.map((r) => r.headers['x-tl-run-id']))).toEqual(new Set([id]));
  });

  it('nested runs form a tree', async () => {
    const c = openaiClient();
    let grandchild = '';
    await guard.run({ runId: 'root', budgetUsd: 10 }, async () => {
      await chat(c);
      await guard.run({ runId: 'child', budgetUsd: 2 }, async () => {
        await chat(c);
        await guard.run(async (g) => {
          grandchild = g.runId;
          await chat(c);
        });
      });
      await chat(c); // back in root after the child finishes
    });
    const h = proxy.requests.map((r) => r.headers);
    expect(h.map((x) => x['x-tl-run-id'])).toEqual(['root', 'child', grandchild, 'root']);
    expect(h.map((x) => x['x-tl-parent-run-id'])).toEqual([undefined, 'root', 'child', undefined]);
    // Budgets belong to their own run only.
    expect(h.map((x) => x['x-tl-run-budget-usd'])).toEqual(['10', '2', undefined, '10']);
  });

  it('run agent applies only when the client has none', async () => {
    const named = openaiClient({ agent: 'client-agent' });
    const unnamed = openaiClient();
    await guard.run({ runId: 'r', agent: 'run-agent' }, async () => {
      await chat(named);
      await chat(unnamed);
      await guard.run('child', () => chat(unnamed));
    });
    expect(proxy.requests.map((r) => r.headers['x-tl-agent'])).toEqual(['client-agent', 'run-agent', undefined]);
  });

  it('sends the user from the client or the run, inherited by child runs', async () => {
    const named = openaiClient({ user: 'alice' });
    const unnamed = openaiClient();
    await guard.run({ runId: 'u1', user: 'bob' }, async () => {
      await chat(named);
      await chat(unnamed);
      await guard.run('u2', () => chat(unnamed));
    });
    await chat(unnamed);
    expect(proxy.requests.map((r) => r.headers['x-tl-user'])).toEqual(['alice', 'bob', 'bob', undefined]);
    expect(() => guard.run({ user: 'not valid' }, () => 0)).toThrow(TypeError);
  });

  it('explicit request headers win', async () => {
    const c = openaiClient({ agent: 'bot' });
    await guard.run('r', () => chat(c, {}, { 'x-tl-run-id': 'manual', 'x-tl-agent': 'manual-agent' }));
    expect(proxy.last().headers['x-tl-run-id']).toBe('manual');
    expect(proxy.last().headers['x-tl-agent']).toBe('manual-agent');
  });

  it('a run cannot be its own parent', () => {
    expect(() => guard.run('same', () => guard.run('same', () => 0))).toThrow(/own parent/);
  });

  it('concurrent runs are isolated', async () => {
    proxy.delayMs = 15;
    const oai = openaiClient({ agent: 'oai' });
    const ant = anthropicClient({ agent: 'ant' });
    const worker = (i: number) =>
      guard.run({ runId: `task-${i}`, budgetUsd: i + 1 }, async () => {
        for (let step = 0; step < 3; step++) {
          const tag = `task-${i}/${step}`;
          if (i % 2) await message(ant, tag);
          else await chat(oai, { messages: [{ role: 'user', content: tag }] });
          await guard.run(`task-${i}-sub-${step}`, () =>
            chat(oai, { messages: [{ role: 'user', content: `${tag}/sub` }] }),
          );
          await new Promise((r) => setTimeout(r, Math.random() * 5));
        }
      });
    await Promise.all(Array.from({ length: 12 }, (_, i) => worker(i)));

    expect(proxy.requests).toHaveLength(12 * 3 * 2);
    for (const { body, headers: h } of proxy.requests) {
      const tag: string = body.messages[0].content;
      const [task, step] = tag.split('/');
      const i = Number(task!.split('-')[1]);
      if (tag.endsWith('/sub')) {
        expect(h['x-tl-run-id']).toBe(`${task}-sub-${step}`);
        expect(h['x-tl-parent-run-id']).toBe(task);
        expect(h['x-tl-run-budget-usd']).toBeUndefined();
      } else {
        expect(h['x-tl-run-id']).toBe(task);
        expect(h['x-tl-parent-run-id']).toBeUndefined();
        expect(Number(h['x-tl-run-budget-usd'])).toBe(i + 1);
      }
    }
    expect(currentRun()).toBeUndefined();
  });
});

describe('error mapping', () => {
  it('402 OpenAI shape -> BudgetExceeded, still an OpenAI.APIError, not retried', async () => {
    proxy.error = { status: 402, type: 'tokenlens_budget_exceeded', message: 'TokenLens budget exceeded for run r1' };
    const c = openaiClient();
    const e = await guard.run('r1', () => caught(chat(c)));
    expect(e).toBeInstanceOf(BudgetExceeded);
    expect(e).toBeInstanceOf(TokenLensError);
    expect(e).toBeInstanceOf(OpenAI.APIError);
    expect(e).not.toBeInstanceOf(RunStopped);
    expect(e.runId).toBe('r1');
    expect(e.status).toBe(402);
    expect(e.errorType).toBe('tokenlens_budget_exceeded');
    expect(e.message).toBe('TokenLens budget exceeded for run r1');
    expect(e.name).toBe('BudgetExceeded');
    expect(String(e.stack)).toMatch(/^BudgetExceeded: TokenLens budget exceeded/);
    expect(proxy.requests).toHaveLength(1);
  });

  it('402 Anthropic shape -> BudgetExceeded', async () => {
    proxy.error = { status: 402, type: 'tokenlens_budget_exceeded', message: 'over budget' };
    const c = anthropicClient();
    const e = await guard.run('r2', () => caught(message(c)));
    expect(e).toBeInstanceOf(BudgetExceeded);
    expect(e).toBeInstanceOf(Anthropic.APIError);
    expect(e.runId).toBe('r2');
    expect(e.message).toBe('over budget');
  });

  for (const [label, make, call, Sdk] of [
    ['openai', openaiClient, chat, OpenAI],
    ['anthropic', anthropicClient, message, Anthropic],
  ] as const) {
    it(`403 paused and killed (${label})`, async () => {
      const c = make() as any;
      proxy.error = { status: 403, type: 'tokenlens_run_paused', message: 'TokenLens: run r3 is paused' };
      const paused = await guard.run('r3', () => caught((call as any)(c)));
      expect(paused).toBeInstanceOf(RunPaused);
      expect(paused).toBeInstanceOf(RunStopped);
      expect(paused).not.toBeInstanceOf(RunKilled);
      expect(paused).toBeInstanceOf(Sdk.PermissionDeniedError);
      expect(paused.runId).toBe('r3');
      expect(paused.status).toBe(403);
      expect(paused.message).toBe('TokenLens: run r3 is paused');

      proxy.error = { status: 403, type: 'tokenlens_run_killed', message: 'TokenLens: run r3 is killed' };
      const killed = await guard.run('r3', () => caught((call as any)(c)));
      expect(killed).toBeInstanceOf(RunKilled);
      expect(killed).not.toBeInstanceOf(RunPaused);
      expect(killed.errorType).toBe('tokenlens_run_killed');
    });
  }

  it('falls back to the proxy-generated run id', async () => {
    proxy.error = { status: 402, type: 'tokenlens_budget_exceeded', message: 'over' };
    const e = await caught(chat(openaiClient()));
    expect(e).toBeInstanceOf(BudgetExceeded);
    expect(e.runId).toBe('req_generated');
  });

  it('leaves other errors to the SDK', async () => {
    proxy.error = { status: 403, type: 'permission_error', message: 'nope' };
    const e = await caught(chat(openaiClient()));
    expect(e).toBeInstanceOf(OpenAI.PermissionDeniedError);
    expect(e).not.toBeInstanceOf(TokenLensError);
    proxy.error = { status: 402, type: 'billing', message: 'pay up' };
    const e2 = await caught(message(anthropicClient()));
    expect(e2).toBeInstanceOf(Anthropic.APIError);
    expect(e2).not.toBeInstanceOf(TokenLensError);
  });

  it('streaming calls throw the mapped error', async () => {
    proxy.error = { status: 402, type: 'tokenlens_budget_exceeded', message: 'over' };
    const c = openaiClient();
    const e = await caught(c.chat.completions.create({ model: 'gpt-4o-mini', messages: MSG, stream: true }));
    expect(e).toBeInstanceOf(BudgetExceeded);
  });
});

describe('local fallback cap', () => {
  it('blocks before sending', async () => {
    const c = openaiClient();
    await guard.run({ runId: 'capped', maxCalls: 2 }, async (run) => {
      await chat(c);
      await chat(c);
      const e = await caught(chat(c));
      expect(e).toBeInstanceOf(LocalCapExceeded);
      expect(e).toBeInstanceOf(BudgetExceeded);
      expect(e.runId).toBe('capped');
      expect(e.status).toBeUndefined();
      expect(run.calls).toBe(2);
    });
    expect(proxy.requests).toHaveLength(2);
  });

  it('child calls count against the parent cap', async () => {
    const c = openaiClient();
    await guard.run({ runId: 'parent', maxCalls: 3 }, async (parent) => {
      await chat(c);
      await guard.run('child', async (child) => {
        await chat(c);
        await chat(c);
        const e = await caught(chat(c));
        expect(e).toBeInstanceOf(LocalCapExceeded);
        expect(e.runId).toBe('parent');
        expect(child.calls).toBe(2);
      });
      expect(parent.calls).toBe(3);
    });
    expect(proxy.requests).toHaveLength(3);
  });

  it('SDK retries do not count against the cap', async () => {
    proxy.error = { status: 500, type: 'server_error', message: 'boom' };
    const c = guard.openai(new OpenAI({ apiKey: 'x', maxRetries: 2 }));
    await guard.run({ runId: 'retry', maxCalls: 1 }, async (run) => {
      const e = await caught(chat(c));
      expect(e).toBeInstanceOf(OpenAI.InternalServerError);
      expect(run.calls).toBe(1);
    });
    expect(proxy.requests).toHaveLength(3);
  });
});

describe('re-wrapping', () => {
  it('does not stack guards; the new agent wins', async () => {
    const c = guard.openai(openaiClient({ agent: 'first' }), { agent: 'second' });
    await guard.run({ runId: 'rw', maxCalls: 1 }, async (run) => {
      await chat(c);
      expect(run.calls).toBe(1);
    });
    expect(proxy.last().headers['x-tl-agent']).toBe('second');
  });
});

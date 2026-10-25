/** Direct mode: the client calls its provider; Tokenrein gets a check before and usage after. */

import { createServer, type IncomingHttpHeaders, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';

import Anthropic from '@anthropic-ai/sdk';
import OpenAI from 'openai';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { BudgetExceeded, RunPaused, flush, guard } from '../src/index.js';
import { providerFor, usageFrom, StreamUsage } from '../src/direct.js';

interface Seen {
  path: string;
  headers: IncomingHttpHeaders;
  body: any;
}

const CHAT = {
  id: 'c1', object: 'chat.completion', created: 0, model: 'gpt-4o-mini',
  choices: [{ index: 0, message: { role: 'assistant', content: 'ok' }, finish_reason: 'stop' }],
  usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15, prompt_tokens_details: { cached_tokens: 2 } },
};
const MESSAGE = {
  id: 'm1', type: 'message', role: 'assistant', model: 'claude-haiku-5-5', content: [{ type: 'text', text: 'ok' }],
  stop_reason: 'end_turn', stop_sequence: null,
  usage: { input_tokens: 20, output_tokens: 7, cache_read_input_tokens: 100, cache_creation_input_tokens: 3 },
};
const sse = (events: [string | null, unknown][]) =>
  events.map(([name, data]) => (name ? `event: ${name}\n` : '') + `data: ${typeof data === 'string' ? data : JSON.stringify(data)}\n\n`).join('');
const CHAT_STREAM = sse([
  [null, { id: 'c1', object: 'chat.completion.chunk', created: 0, model: 'gpt-4o-mini', choices: [{ index: 0, delta: { content: 'o' }, finish_reason: null }] }],
  [null, { id: 'c1', object: 'chat.completion.chunk', created: 0, model: 'gpt-4o-mini', choices: [], usage: { prompt_tokens: 30, completion_tokens: 9, total_tokens: 39 } }],
  [null, '[DONE]'],
]);
const MESSAGE_STREAM = sse([
  ['message_start', { type: 'message_start', message: { id: 'm1', type: 'message', role: 'assistant', model: 'claude-haiku-5-5', content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: 40, output_tokens: 1, cache_read_input_tokens: 500, cache_creation_input_tokens: 0 } } }],
  ['content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } }],
  ['content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: 'hi' } }],
  ['content_block_stop', { type: 'content_block_stop', index: 0 }],
  ['message_delta', { type: 'message_delta', delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: { output_tokens: 12 } }],
  ['message_stop', { type: 'message_stop' }],
]);

class Fake {
  seen: Seen[] = [];
  url = '';
  #server: Server;
  constructor(reply: (path: string, body: any) => [number, string, string]) {
    this.#server = createServer((req, res) => {
      const chunks: Buffer[] = [];
      req.on('data', (c: Buffer) => chunks.push(c));
      req.on('end', () => {
        const body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString()) : {};
        this.seen.push({ path: req.url ?? '', headers: req.headers, body });
        const [status, ctype, data] = reply(req.url ?? '', body);
        res.writeHead(status, { 'content-type': ctype });
        res.end(data);
      });
    });
  }
  async start() {
    await new Promise<void>((r) => this.#server.listen(0, '127.0.0.1', r));
    this.url = `http://127.0.0.1:${(this.#server.address() as AddressInfo).port}`;
    return this;
  }
  async close() {
    this.#server.closeAllConnections();
    await new Promise<void>((r) => this.#server.close(() => r()));
  }
}

let provider: Fake;
let tl: Fake;
let verdict: any;
let over: string[];
const checks = () => tl.seen.filter((s) => s.path === '/v1/check').map((s) => s.body);
const usage = async () => {
  await flush();
  return tl.seen.filter((s) => s.path === '/v1/usage').flatMap((s) => s.body.events);
};
const MSG = [{ role: 'user' as const, content: 'hi' }];

beforeEach(async () => {
  verdict = { allowed: true };
  over = [];
  provider = await new Fake((path, body) => {
    const streaming = body?.stream === true;
    if (path.endsWith('/messages')) return streaming ? [200, 'text/event-stream', MESSAGE_STREAM] : [200, 'application/json', JSON.stringify(MESSAGE)];
    return streaming ? [200, 'text/event-stream', CHAT_STREAM] : [200, 'application/json', JSON.stringify(CHAT)];
  }).start();
  tl = await new Fake((path, body) => {
    if (path === '/v1/check') return [200, 'application/json', JSON.stringify(verdict)];
    if (path === '/v1/usage') return [200, 'application/json', JSON.stringify({ accepted: body.events.length, over })];
    return [404, 'application/json', '{}'];
  }).start();
  process.env.TOKENREIN_BASE_URL = tl.url;
  process.env.TOKENREIN_API_KEY = 'tl_direct_key';
  for (const n of ['API_KEY', 'BASE_URL', 'MODE']) delete process.env[`TOKENLENS_${n}`]; // pre-rename names
  delete process.env.TOKENREIN_MODE;
});
afterEach(async () => {
  await provider.close();
  await tl.close().catch(() => {});
});

describe('direct mode', () => {
  it('calls the provider directly and reports usage to Tokenrein', async () => {
    const client = guard.openai(new OpenAI({ apiKey: 'sk-provider', baseURL: `${provider.url}/v1` }), { agent: 'writer', user: 'ana@example.com' });
    await guard.run({ runId: 'job-1', budgetUsd: 2 }, () =>
      guard.run('job-1-sub', () => client.chat.completions.create({ model: 'gpt-4o-mini', messages: MSG, max_tokens: 50 })),
    );
    const call = provider.seen[0]!;
    expect(call.headers.authorization).toBe('Bearer sk-provider');
    expect(Object.keys(call.headers).some((h) => h.startsWith('x-tr-'))).toBe(false);
    expect(checks()[0]).toMatchObject({ run_id: 'job-1-sub', parent_run_id: 'job-1', agent: 'writer', user: 'ana@example.com', model: 'gpt-4o-mini', max_output_tokens: 50 });
    const [ev] = await usage();
    expect(ev).toMatchObject({ run_id: 'job-1-sub', parent_run_id: 'job-1', input_tokens: 8, cache_read_tokens: 2, output_tokens: 5, model: 'gpt-4o-mini', source: 'typescript-sdk', stream: false });
    expect(tl.seen.at(-1)!.headers.authorization).toBe('Bearer tl_direct_key');
  });

  it('reads Anthropic stream usage', async () => {
    const client = guard.anthropic(new Anthropic({ apiKey: 'sk-ant', baseURL: provider.url }), { agent: 'bot' });
    const stream = client.messages.stream({ model: 'claude-haiku-5-5', max_tokens: 64, messages: MSG });
    let text = '';
    for await (const ev of stream) if (ev.type === 'content_block_delta' && ev.delta.type === 'text_delta') text += ev.delta.text;
    expect(text).toBe('hi');
    await new Promise((r) => setTimeout(r, 20));
    const [ev] = await usage();
    expect(ev).toMatchObject({ input_tokens: 40, cache_read_tokens: 500, output_tokens: 12, model: 'claude-haiku-5-5', stream: true });
  });

  it('asks OpenAI-style streams for usage', async () => {
    const client = guard.openai(new OpenAI({ apiKey: 'sk', baseURL: `${provider.url}/v1` }));
    const s = await client.chat.completions.create({ model: 'gpt-4o-mini', messages: MSG, stream: true });
    for await (const _ of s) {
      // drain
    }
    expect(provider.seen[0]!.body.stream_options).toEqual({ include_usage: true });
    await new Promise((r) => setTimeout(r, 20));
    const [ev] = await usage();
    expect(ev).toMatchObject({ input_tokens: 30, output_tokens: 9 });
  });

  it('refused calls never reach the provider', async () => {
    const client = guard.openai(new OpenAI({ apiKey: 'sk', baseURL: `${provider.url}/v1`, maxRetries: 0 }));
    verdict = { allowed: false, reason: 'Tokenrein: the run:job-2 budget is used up', scope: 'run:job-2', run_id: 'job-2' };
    const e = await guard.run('job-2', () => client.chat.completions.create({ model: 'gpt-4o-mini', messages: MSG })).catch((x) => x);
    expect(e).toBeInstanceOf(BudgetExceeded);
    expect(e.runId).toBe('job-2');
    verdict = { allowed: false, reason: 'paused', run_state: 'paused', run_id: 'job-2' };
    await expect(client.chat.completions.create({ model: 'gpt-4o-mini', messages: MSG })).rejects.toBeInstanceOf(RunPaused);
    expect(provider.seen).toHaveLength(0);
  });

  it('without checks, reported overruns stop the run', async () => {
    const client = guard.openai(new OpenAI({ apiKey: 'sk', baseURL: `${provider.url}/v1` }), { check: false });
    over = ['run:job-3'];
    await guard.run('job-3', async () => {
      await client.chat.completions.create({ model: 'gpt-4o-mini', messages: MSG });
      await usage();
      await expect(client.chat.completions.create({ model: 'gpt-4o-mini', messages: MSG })).rejects.toBeInstanceOf(BudgetExceeded);
    });
    expect(checks()).toHaveLength(0);
    expect(provider.seen).toHaveLength(1);
  });

  it('Tokenrein being down never breaks calls', async () => {
    await tl.close();
    const client = guard.openai(new OpenAI({ apiKey: 'sk', baseURL: `${provider.url}/v1` }), { apiKey: 'tl_other' });
    const out = await client.chat.completions.create({ model: 'gpt-4o-mini', messages: MSG });
    expect(out.choices[0]!.message.content).toBe('ok');
  });

  it('keeps runs separate across concurrent calls', async () => {
    const client = guard.anthropic(new Anthropic({ apiKey: 'sk', baseURL: provider.url }));
    await Promise.all(
      [1, 2, 3].map((i) => guard.run(`r${i}`, () => client.messages.create({ model: 'claude-haiku-5-5', max_tokens: 10, messages: MSG }))),
    );
    const events = await usage();
    expect(events.map((e) => e.run_id).sort()).toEqual(['r1', 'r2', 'r3']);
    expect(events[0]).toMatchObject({ cache_write_tokens: 3, provider: 'anthropic' });
  });
});

describe('helpers', () => {
  it('names providers from base URLs', () => {
    expect(providerFor('https://api.openai.com/v1', 'x')).toBe('openai');
    expect(providerFor('https://generativelanguage.googleapis.com/v1beta/openai/', 'x')).toBe('google');
    expect(providerFor('https://api.groq.com/openai/v1', 'x')).toBe('groq');
    expect(providerFor('http://localhost:11434/v1', 'x')).toBe('ollama');
    expect(providerFor('http://localhost:4000/v1', 'openai')).toBe('openai');
    expect(providerFor('https://llm.internal.example', 'openai')).toBe('openai');
  });
  it('reads usage shapes', () => {
    expect(usageFrom({ usage: { input_tokens: 100, output_tokens: 5, input_tokens_details: { cached_tokens: 40 } } })).toEqual({ input_tokens: 60, output_tokens: 5, cache_read_tokens: 40, cache_write_tokens: 0 });
    const s = new StreamUsage();
    s.feed('data: {"type":"response.completed","response":{"model":"gpt-5","usage":{"input_tokens":9,"output_tokens":3,"output_tokens_details":{}}}}\n');
    expect(s.model).toBe('gpt-5');
    expect(s.usage?.output_tokens).toBe(3);
  });
});

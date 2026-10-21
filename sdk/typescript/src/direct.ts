/**
 * Direct mode: the wrapped client calls its provider itself; TokenLens gets usage only.
 *
 * Before each call the SDK asks TokenLens whether the run may continue (`POST /v1/check`);
 * after each response it reads the token usage (also from streams) and reports it in the
 * background (`POST /v1/usage`). Prompts and responses never leave the process. Problems
 * reaching TokenLens never break a call: checks fail open and reports are dropped after retries.
 */

import { BUDGET_EXCEEDED, BudgetExceeded, RUN_KILLED, RUN_PAUSED, RunKilled, RunPaused, TokenLensError } from './errors.js';

const HOST_PROVIDERS: Record<string, string> = {
  'api.openai.com': 'openai',
  'api.anthropic.com': 'anthropic',
  'generativelanguage.googleapis.com': 'google',
  'api.groq.com': 'groq',
  'api.mistral.ai': 'mistral',
  'api.deepseek.com': 'deepseek',
  'api.x.ai': 'xai',
  'openrouter.ai': 'openrouter',
  'api.together.xyz': 'together',
  'api.fireworks.ai': 'fireworks',
  'api.cohere.ai': 'cohere',
  'api.cohere.com': 'cohere',
};

/** Name the provider from the client's base URL (OpenAI-compatible clients reach many). */
export function providerFor(baseURL: string, fallback: string): string {
  let url: URL;
  try {
    url = new URL(baseURL);
  } catch {
    return fallback;
  }
  const host = url.hostname.toLowerCase();
  if (HOST_PROVIDERS[host]) return HOST_PROVIDERS[host]!;
  if (host.endsWith('.openai.azure.com')) return 'openai';
  if (['localhost', '127.0.0.1', '[::1]', '::1'].includes(host)) {
    // Well-known local model servers are free; any other local address may be a gateway to
    // paid models, so it is priced by model name.
    return ({ '11434': 'ollama', '1234': 'lmstudio', '8000': 'vllm' } as Record<string, string>)[url.port] ?? fallback;
  }
  return fallback;
}

export interface TokenUsage {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
}

const num = (v: unknown): number => (typeof v === 'number' && Number.isFinite(v) ? v : 0);

/** Token counts from an OpenAI- or Anthropic-shaped object with a `usage` field. */
export function usageFrom(obj: unknown): TokenUsage | undefined {
  const u = (obj as { usage?: Record<string, unknown> } | null | undefined)?.usage;
  if (!u || typeof u !== 'object') return undefined;
  const nested = (a: string, b: string) => num((u[a] as Record<string, unknown> | undefined)?.[b]);
  if ('prompt_tokens' in u) {
    const cached = nested('prompt_tokens_details', 'cached_tokens');
    return { input_tokens: Math.max(0, num(u.prompt_tokens) - cached), output_tokens: num(u.completion_tokens), cache_read_tokens: cached, cache_write_tokens: 0 };
  }
  if ('input_tokens_details' in u || 'output_tokens_details' in u) {
    const cached = nested('input_tokens_details', 'cached_tokens');
    return { input_tokens: Math.max(0, num(u.input_tokens) - cached), output_tokens: num(u.output_tokens), cache_read_tokens: cached, cache_write_tokens: 0 };
  }
  if ('input_tokens' in u || 'output_tokens' in u) {
    return {
      input_tokens: num(u.input_tokens),
      output_tokens: num(u.output_tokens),
      cache_read_tokens: num(u.cache_read_input_tokens),
      cache_write_tokens: num(u.cache_creation_input_tokens),
    };
  }
  return undefined;
}

/** Accumulates usage from server-sent events. */
export class StreamUsage {
  model: string | undefined;
  usage: TokenUsage | undefined;
  #buf = '';

  feed(text: string): void {
    this.#buf += text;
    let i: number;
    while ((i = this.#buf.indexOf('\n')) >= 0) {
      const line = this.#buf.slice(0, i).trim();
      this.#buf = this.#buf.slice(i + 1);
      this.#line(line);
    }
  }

  #line(line: string): void {
    if (!line.startsWith('data:')) return;
    const data = line.slice(5).trim();
    if (!data || data === '[DONE]') return;
    let ev: Record<string, any>;
    try {
      ev = JSON.parse(data);
    } catch {
      return;
    }
    if (!ev || typeof ev !== 'object') return;
    if (ev.type === 'message_start') {
      this.model = ev.message?.model ?? this.model;
      this.usage = usageFrom(ev.message) ?? this.usage;
    } else if (ev.type === 'message_delta') {
      if (typeof ev.usage?.output_tokens === 'number') {
        this.usage = { input_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0, ...this.usage, output_tokens: ev.usage.output_tokens };
      }
    } else if (ev.type === 'response.completed') {
      this.model = ev.response?.model ?? this.model;
      this.usage = usageFrom(ev.response) ?? this.usage;
    } else {
      this.model = ev.model ?? this.model;
      this.usage = usageFrom(ev) ?? this.usage;
    }
  }
}

export interface Verdict {
  allowed: boolean;
  reason?: string;
  scope?: string;
  run_state?: string | null;
  run_id?: string;
}

/** Talks to TokenLens: budget checks and batched usage reports. */
export class Reporter {
  /** Enforced limits that recent reports said are used up: label -> time noted. */
  readonly over = new Map<string, number>();
  #queue: Record<string, unknown>[] = [];
  #timer: ReturnType<typeof setTimeout> | undefined;
  #inflight: Promise<void> = Promise.resolve();

  constructor(
    readonly baseURL: string,
    readonly apiKey: string,
    readonly checkTimeoutMs = 2000,
  ) {}

  async #post(path: string, body: unknown, timeoutMs: number): Promise<Response> {
    return globalThis.fetch(this.baseURL + path, {
      method: 'POST',
      headers: { Authorization: `Bearer ${this.apiKey}`, 'content-type': 'application/json' },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(timeoutMs),
    });
  }

  /** Fails open when TokenLens can't be reached. */
  async check(body: Record<string, unknown>): Promise<Verdict | undefined> {
    let res: Response;
    try {
      res = await this.#post('/v1/check', body, this.checkTimeoutMs);
    } catch {
      return undefined;
    }
    if (res.status === 401) {
      throw new TokenLensError('TokenLens rejected the API key (TOKENLENS_API_KEY)', { errorType: 'tokenlens_unauthorized', status: 401 });
    }
    if (!res.ok) return undefined;
    try {
      return (await res.json()) as Verdict;
    } catch {
      return undefined;
    }
  }

  report(event: Record<string, unknown>): void {
    if (this.#queue.length >= 10_000) return;
    this.#queue.push(event);
    if (!this.#timer) {
      this.#timer = setTimeout(() => {
        this.#timer = undefined;
        this.#inflight = this.#inflight.then(() => this.#send());
      }, 200);
      this.#timer.unref?.();
    }
  }

  /** Send queued reports now. */
  async flush(): Promise<void> {
    if (this.#timer) {
      clearTimeout(this.#timer);
      this.#timer = undefined;
      this.#inflight = this.#inflight.then(() => this.#send());
    }
    await this.#inflight;
  }

  async #send(): Promise<void> {
    while (this.#queue.length) {
      const batch = this.#queue.splice(0, 100);
      for (let attempt = 0; attempt < 3; attempt++) {
        try {
          const res = await this.#post('/v1/usage', { events: batch }, 10_000);
          if (res.ok) {
            const out = (await res.json()) as { over?: string[] };
            const now = Date.now();
            for (const label of out.over ?? []) this.over.set(label, now);
            break;
          }
          if (res.status < 500) break;
        } catch {
          // retry below
        }
        await new Promise((r) => setTimeout(r, 300 * (attempt + 1)));
      }
    }
  }
}

const reporters = new Map<string, Reporter>();

export function reporterFor(baseURL: string, apiKey: string): Reporter {
  const key = `${baseURL}\u0000${apiKey}`;
  let r = reporters.get(key);
  if (!r) {
    r = new Reporter(baseURL, apiKey);
    reporters.set(key, r);
  }
  return r;
}

/** Send every queued usage report now (call before a short-lived process exits). */
export async function flush(): Promise<void> {
  await Promise.all([...reporters.values()].map((r) => r.flush()));
}

export function verdictError(v: Verdict, runId: string | undefined): TokenLensError {
  const message = v.reason ?? 'TokenLens refused the call';
  const id = v.run_id ?? runId;
  if (v.run_state === 'killed') return new RunKilled(message, { runId: id, errorType: RUN_KILLED });
  if (v.run_state === 'paused') return new RunPaused(message, { runId: id, errorType: RUN_PAUSED });
  return new BudgetExceeded(message, { runId: id, errorType: BUDGET_EXCEEDED });
}

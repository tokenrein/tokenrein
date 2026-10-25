/**
 * Wrap OpenAI / Anthropic SDK clients for Tokenrein.
 *
 * Two modes:
 *  - **direct** (default): the client keeps calling its provider. Before each call it asks
 *    Tokenrein whether the run may continue; after each response it reports token usage in the
 *    background. Works with any provider the `openai` SDK reaches (Gemini, Groq, Mistral,
 *    DeepSeek, OpenRouter, Ollama, ...). See `direct.ts`.
 *  - **proxy**: the client calls the Tokenrein proxy, which checks every call before sending it.
 *
 * In proxy mode the guarded client is a `withOptions()` copy of the caller's client whose prototype is
 * swapped for a subclass that:
 *  - adds the `x-tr-*` headers of the current run to every request it builds, and counts
 *    the call against any local `maxCalls` cap;
 *  - re-labels the proxy's 402/403 responses as Tokenrein errors.
 * Later `withOptions()` copies construct the same subclass, so they stay guarded.
 */

import { envVar } from './env.js';
import {
  type TokenUsage,
  type Verdict,
  StreamUsage,
  providerFor,
  reporterFor,
  usageFrom,
  verdictError,
} from './direct.js';
import { BUDGET_EXCEEDED, RUN_KILLED, RUN_PAUSED, relabel } from './errors.js';
import { currentRun, validateId, validateUser } from './run.js';

export const DEFAULT_BASE_URL = 'http://127.0.0.1:8787';

export interface GuardOptions {
  /** Agent name sent as `x-tr-agent` (per-agent daily budget). */
  agent?: string;
  /** Person or service the agent works for, sent as `x-tr-user` (spend reports). */
  user?: string;
  /** Tokenrein virtual key; defaults to `process.env.TOKENREIN_API_KEY`. */
  apiKey?: string;
  /** Tokenrein URL; defaults to `process.env.TOKENREIN_BASE_URL` or `http://127.0.0.1:8787`. */
  baseURL?: string;
  /**
   * `'direct'` (default, or `process.env.TOKENREIN_MODE`): the client keeps calling its provider;
   * Tokenrein checks each call first and receives token usage afterwards, never prompts.
   * `'proxy'`: calls go through the Tokenrein proxy, which checks every call before sending it.
   */
  mode?: 'direct' | 'proxy';
  /**
   * Direct mode only. Ask Tokenrein before each call (default). `false` skips the round trip
   * and only stops runs that previous usage reports showed are over an enforced limit.
   */
  check?: boolean;
}

/** The parts of a Stainless-generated SDK client (openai, @anthropic-ai/sdk) this module uses. */
interface SdkClient {
  buildRequest: Function;
  makeStatusError: Function;
  /** openai >= 5, @anthropic-ai/sdk >= 0.40. */
  withOptions?(options: Record<string, unknown>): SdkClient;
  /** Constructor options, used on older SDKs without `withOptions`. */
  _options?: Record<string, unknown>;
}

type HeaderBag = Headers | Record<string, string | null | undefined>;

interface BuiltRequest {
  req: { headers?: HeaderBag };
  url: string;
  timeout: number;
}

type ClientClass = (new (...args: any[]) => any) & { [UNGUARDED]?: ClientClass };

/** On a guarded class: the SDK class it extends (so re-wrapping does not stack guards). */
const UNGUARDED = Symbol('tokenrein.unguarded');

export function resolveBaseURL(baseURL?: string): string {
  let url = (baseURL || envVar('BASE_URL') || DEFAULT_BASE_URL).replace(/\/+$/, '');
  // Accept either the proxy root or its /v1 URL.
  if (url.endsWith('/v1')) url = url.slice(0, -'/v1'.length);
  return url;
}

export function resolveApiKey(apiKey?: string): string {
  const key = apiKey || envVar('API_KEY');
  if (!key) throw new TypeError('Tokenrein virtual key missing: pass apiKey or set TOKENREIN_API_KEY');
  return key;
}

function hasHeader(h: HeaderBag, name: string): boolean {
  if (typeof (h as Headers).has === 'function') return (h as Headers).has(name);
  return Object.keys(h).some((k) => k.toLowerCase() === name && (h as Record<string, unknown>)[k] != null);
}

function getHeader(h: HeaderBag | undefined, name: string): string | undefined {
  if (!h) return undefined;
  if (typeof (h as Headers).get === 'function') return (h as Headers).get(name) ?? undefined;
  const k = Object.keys(h).find((k) => k.toLowerCase() === name);
  return k ? ((h as Record<string, string | undefined>)[k] ?? undefined) : undefined;
}

function setHeader(h: HeaderBag, name: string, value: string): void {
  if (typeof (h as Headers).set === 'function') (h as Headers).set(name, value);
  else (h as Record<string, string>)[name] = value;
}

function deleteHeader(h: HeaderBag, name: string): void {
  if (typeof (h as Headers).delete === 'function') {
    (h as Headers).delete(name);
    return;
  }
  for (const k of Object.keys(h)) if (k.toLowerCase() === name) delete (h as Record<string, unknown>)[k];
}

function tlHeaders(agent: string | undefined, user: string | undefined): Record<string, string> {
  const run = currentRun();
  const h: Record<string, string> = run ? run.headers() : {};
  const name = agent ?? run?.agent;
  if (name) h['x-tr-agent'] = name;
  const who = user ?? run?.user;
  if (who) h['x-tr-user'] = who;
  return h;
}

function guardedClass(
  Base: ClientClass,
  agent: string | undefined,
  user: string | undefined,
  stripBearer: boolean,
): ClientClass {
  // Request options objects already counted. SDK retries reuse the options object; older
  // SDKs (openai < 4.6x) do not pass `retryCount`, so this is how their retries are skipped.
  const counted = new WeakSet<object>();
  const guardRequest = (built: BuiltRequest, options: unknown, opts: { retryCount?: number }): BuiltRequest => {
    const req = built.req;
    req.headers ??= {};
    for (const [name, value] of Object.entries(tlHeaders(agent, user))) {
      // Headers the caller set explicitly (request headers / defaultHeaders) win.
      if (!hasHeader(req.headers, name)) setHeader(req.headers, name, value);
    }
    if (stripBearer) {
      // The proxy reads `Authorization: Bearer` before `x-api-key`; never forward a provider
      // token (authToken, ANTHROPIC_AUTH_TOKEN, credential providers) in place of the Tokenrein key.
      deleteHeader(req.headers, 'authorization');
    }
    const first = !opts.retryCount && !(typeof options === 'object' && options !== null && counted.has(options));
    if (first) {
      currentRun()?.recordCall();
      if (typeof options === 'object' && options !== null) counted.add(options);
    }
    return built;
  };

  return class Guarded extends Base {
    static [UNGUARDED] = Base;

    static override get name(): string {
      return Base.name;
    }

    // Newer SDKs build requests asynchronously, older ones (openai v4) synchronously.
    buildRequest(options: unknown, opts: { retryCount?: number } = {}): BuiltRequest | Promise<BuiltRequest> {
      const built: BuiltRequest | Promise<BuiltRequest> = super.buildRequest(options, opts);
      return built instanceof Promise
        ? built.then((b) => guardRequest(b, options, opts))
        : guardRequest(built, options, opts);
    }

    makeStatusError(status: number, error: unknown, message: string | undefined, headers: HeaderBag): Error {
      const err: Error = super.makeStatusError(status, error, message, headers);
      // The proxy echoes the run id; fall back to the one this request carried.
      const runId = getHeader(headers, 'x-tr-run-id') ?? currentRun()?.runId;
      return relabel(err, status, error, runId);
    }
  };
}

export function wrap<T>(client: T, opts: GuardOptions, provider: 'openai' | 'anthropic'): T {
  const c = client as unknown as SdkClient;
  if (!c || typeof c.buildRequest !== 'function' || typeof c.makeStatusError !== 'function') {
    throw new TypeError(`guard.${provider}() needs an ${provider === 'openai' ? 'OpenAI' : 'Anthropic'} client instance`);
  }
  if (opts.agent !== undefined) validateId(opts.agent, 'agent');
  if (opts.user !== undefined) validateUser(opts.user);
  const stripBearer = provider === 'anthropic';
  const root = resolveBaseURL(opts.baseURL);
  const overrides = {
    apiKey: resolveApiKey(opts.apiKey),
    baseURL: provider === 'openai' ? `${root}/v1` : root,
    ...(stripBearer ? { authToken: null } : {}),
  };
  const Base = (c.constructor as ClientClass)[UNGUARDED] ?? (c.constructor as ClientClass);
  const Guarded = guardedClass(Base, opts.agent, opts.user, stripBearer);
  if (typeof c.withOptions !== 'function') {
    return new Guarded({ ...c._options, ...overrides }) as T;
  }
  const copy = c.withOptions(overrides);
  Object.setPrototypeOf(copy, Guarded.prototype);
  return copy as unknown as T;
}

// --- Direct mode ---------------------------------------------------------------------------

const MAX_OUTPUT_FIELDS = ['max_tokens', 'max_completion_tokens', 'max_output_tokens'];

interface DirectConfig {
  agent: string | undefined;
  user: string | undefined;
  provider: string;
  check: boolean;
  reporter: ReturnType<typeof reporterFor>;
}

function who(cfg: DirectConfig): Record<string, unknown> {
  const run = currentRun();
  const out: Record<string, unknown> = { source: 'typescript-sdk' };
  if (run) {
    out.run_id = run.runId;
    if (run.parent) out.parent_run_id = run.parent.runId;
    if (run.budgetUsd !== undefined) out.run_budget_usd = run.budgetUsd;
  }
  const agent = cfg.agent ?? run?.agent;
  const user = cfg.user ?? run?.user;
  if (agent) out.agent = agent;
  if (user) out.user = user;
  return out;
}

function denyIfOver(cfg: DirectConfig): void {
  const run = currentRun();
  const agent = cfg.agent ?? run?.agent;
  for (const label of cfg.reporter.over.keys()) {
    const [kind, rest = ''] = label.split(/:(.*)/s);
    const hit =
      (kind === 'run' && rest === run?.runId) ||
      (kind === 'agent_daily' && agent !== undefined && rest.endsWith(`/${agent}`)) ||
      kind === 'project_monthly' ||
      kind === 'org_monthly';
    if (hit) throw verdictError({ allowed: false, reason: `Tokenrein: the ${label} budget is used up` }, run?.runId);
  }
}

function report(cfg: DirectConfig, w: Record<string, unknown>, model: string | undefined, usage: TokenUsage | undefined, started: number, stream: boolean): void {
  if (!usage) return;
  cfg.reporter.report({ ...w, ...usage, model: model ?? 'unknown', provider: cfg.provider, duration_ms: Date.now() - started, stream });
}

/** Read a response body (web ReadableStream or Node stream) chunk by chunk. */
async function* bodyChunks(body: any): AsyncGenerator<Uint8Array> {
  if (body && typeof body.getReader === 'function') {
    const reader = body.getReader();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return;
      yield value;
    }
  } else if (body && typeof body[Symbol.asyncIterator] === 'function') {
    for await (const chunk of body) yield typeof chunk === 'string' ? new TextEncoder().encode(chunk) : chunk;
  }
}

/** A provider-shaped error response; the SDK raises it and `makeStatusError` relabels it. */
function refusal(v: Verdict, kind: 'openai' | 'anthropic'): Response {
  const type = v.run_state === 'killed' ? RUN_KILLED : v.run_state === 'paused' ? RUN_PAUSED : BUDGET_EXCEEDED;
  const error = { type, message: v.reason ?? 'Tokenrein refused the call' };
  const body = kind === 'anthropic' ? { type: 'error', error } : { error: { ...error, code: type, param: null } };
  const headers: Record<string, string> = { 'content-type': 'application/json' };
  if (v.run_id) headers['x-tr-run-id'] = v.run_id;
  return new Response(JSON.stringify(body), { status: v.run_state ? 403 : 402, headers });
}

function overVerdict(cfg: DirectConfig): Verdict | undefined {
  try {
    denyIfOver(cfg);
    return undefined;
  } catch (e) {
    return { allowed: false, reason: (e as Error).message, run_id: currentRun()?.runId };
  }
}

/**
 * Wraps the client's fetch, which every SDK version supports: checks the budget before the call
 * (refusing with a provider-shaped 402/403 without calling the provider), asks OpenAI-style
 * streams for usage, and reads usage from JSON bodies and event streams.
 */
function observingFetch(inner: typeof fetch, cfg: DirectConfig, kind: 'openai' | 'anthropic'): typeof fetch {
  return async (input: any, init?: any) => {
    const w = who(cfg);
    const url = String(typeof input === 'string' ? input : (input?.url ?? input));
    let body: any;
    if (typeof init?.body === 'string') {
      try {
        body = JSON.parse(init.body);
      } catch {
        body = undefined;
      }
    }
    if (
      cfg.provider !== 'anthropic' &&
      body && body.stream === true && body.stream_options === undefined &&
      new URL(url, 'http://x').pathname.endsWith('/chat/completions')
    ) {
      body.stream_options = { include_usage: true };
      init = { ...init, body: JSON.stringify(body) };
    }
    if (body && typeof body.model === 'string') {
      let v: Verdict | undefined;
      if (cfg.check) {
        const req: Record<string, unknown> = { ...w, model: body.model, provider: cfg.provider };
        const max = MAX_OUTPUT_FIELDS.map((f) => body[f]).find((x) => typeof x === 'number');
        if (max !== undefined) req.max_output_tokens = max;
        v = await cfg.reporter.check(req);
      } else {
        v = overVerdict(cfg);
      }
      if (v && !v.allowed) return refusal({ ...v, run_id: v.run_id ?? (w.run_id as string | undefined) }, kind);
    }
    const started = Date.now();
    const res: any = await inner(input, init);
    if (!res.ok || !res.body) return res;
    const ctype: string = res.headers.get('content-type') ?? '';
    if (ctype.includes('text/event-stream')) {
      const copy = res.clone();
      const parser = new StreamUsage();
      void (async () => {
        const decoder = new TextDecoder();
        try {
          for await (const chunk of bodyChunks(copy.body)) parser.feed(decoder.decode(chunk, { stream: true }));
        } catch {
          // stream aborted; report what was seen
        }
        report(cfg, w, parser.model, parser.usage, started, true);
      })();
    } else if (ctype.includes('json')) {
      res
        .clone()
        .json()
        .then((b: any) => report(cfg, w, b?.model, usageFrom(b), started, false))
        .catch(() => {});
    }
    return res;
  };
}

function directClass(Base: ClientClass): ClientClass {
  return class Direct extends Base {
    static [UNGUARDED] = Base;

    static override get name(): string {
      return Base.name;
    }

    buildRequest(options: unknown, opts: { retryCount?: number } = {}): BuiltRequest | Promise<BuiltRequest> {
      const built: BuiltRequest | Promise<BuiltRequest> = super.buildRequest(options, opts);
      const count = (b: BuiltRequest) => {
        if (!opts.retryCount) currentRun()?.recordCall();
        return b;
      };
      return built instanceof Promise ? built.then(count) : count(built);
    }

    makeStatusError(status: number, error: unknown, message: string | undefined, headers: HeaderBag): Error {
      const err: Error = super.makeStatusError(status, error, message, headers);
      const runId = getHeader(headers, 'x-tr-run-id') ?? currentRun()?.runId;
      return relabel(err, status, error, runId);
    }
  };
}

interface DirectWrapOptions {
  tokenreinURL: string;
  apiKey: string;
  agent: string | undefined;
  user: string | undefined;
  defaultProvider: string;
  check: boolean;
}

export function wrapDirect<T>(client: T, o: DirectWrapOptions, kind: 'openai' | 'anthropic'): T {
  const c = client as unknown as SdkClient & { baseURL?: string; fetch?: typeof fetch };
  if (!c || typeof c.buildRequest !== 'function') {
    throw new TypeError(`guard.${kind}() needs an ${kind === 'openai' ? 'OpenAI' : 'Anthropic'} client instance`);
  }
  if (o.agent !== undefined) validateId(o.agent, 'agent');
  if (o.user !== undefined) validateUser(o.user);
  const cfg: DirectConfig = {
    agent: o.agent,
    user: o.user,
    provider: providerFor(String(c.baseURL ?? ''), o.defaultProvider),
    check: o.check,
    reporter: reporterFor(o.tokenreinURL, o.apiKey),
  };
  const inner: typeof fetch = (c.fetch ?? globalThis.fetch).bind(globalThis);
  const fetchOverride = { fetch: observingFetch(inner, cfg, kind) };
  const Base = (c.constructor as ClientClass)[UNGUARDED] ?? (c.constructor as ClientClass);
  const Direct = directClass(Base);
  if (typeof c.withOptions !== 'function') {
    return new Direct({ ...c._options, ...fetchOverride }) as T;
  }
  const copy = c.withOptions(fetchOverride);
  Object.setPrototypeOf(copy, Direct.prototype);
  return copy as unknown as T;
}

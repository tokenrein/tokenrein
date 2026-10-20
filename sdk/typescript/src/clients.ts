/**
 * Point OpenAI / Anthropic SDK clients at the TokenLens proxy.
 *
 * The guarded client is a `withOptions()` copy of the caller's client whose prototype is
 * swapped for a subclass that:
 *  - adds the `x-tl-*` headers of the current run to every request it builds, and counts
 *    the call against any local `maxCalls` cap;
 *  - re-labels the proxy's 402/403 responses as TokenLens errors.
 * Later `withOptions()` copies construct the same subclass, so they stay guarded.
 */

import { relabel } from './errors.js';
import { currentRun, validateId } from './run.js';

export const DEFAULT_BASE_URL = 'http://127.0.0.1:8787';

export interface GuardOptions {
  /** Agent name sent as `x-tl-agent` (per-agent daily budget). */
  agent?: string;
  /** TokenLens virtual key; defaults to `process.env.TOKENLENS_API_KEY`. */
  apiKey?: string;
  /** Proxy URL; defaults to `process.env.TOKENLENS_BASE_URL` or `http://127.0.0.1:8787`. */
  baseURL?: string;
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
const UNGUARDED = Symbol('tokenlens.unguarded');

export function resolveBaseURL(baseURL?: string): string {
  let url = (baseURL || process.env.TOKENLENS_BASE_URL || DEFAULT_BASE_URL).replace(/\/+$/, '');
  // Accept either the proxy root or its /v1 URL.
  if (url.endsWith('/v1')) url = url.slice(0, -'/v1'.length);
  return url;
}

export function resolveApiKey(apiKey?: string): string {
  const key = apiKey || process.env.TOKENLENS_API_KEY;
  if (!key) throw new TypeError('TokenLens virtual key missing: pass apiKey or set TOKENLENS_API_KEY');
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

function tlHeaders(agent: string | undefined): Record<string, string> {
  const run = currentRun();
  const h: Record<string, string> = run ? run.headers() : {};
  const name = agent ?? run?.agent;
  if (name) h['x-tl-agent'] = name;
  return h;
}

function guardedClass(Base: ClientClass, agent: string | undefined, stripBearer: boolean): ClientClass {
  // Request options objects already counted. SDK retries reuse the options object; older
  // SDKs (openai < 4.6x) do not pass `retryCount`, so this is how their retries are skipped.
  const counted = new WeakSet<object>();
  const guardRequest = (built: BuiltRequest, options: unknown, opts: { retryCount?: number }): BuiltRequest => {
    const req = built.req;
    req.headers ??= {};
    for (const [name, value] of Object.entries(tlHeaders(agent))) {
      // Headers the caller set explicitly (request headers / defaultHeaders) win.
      if (!hasHeader(req.headers, name)) setHeader(req.headers, name, value);
    }
    if (stripBearer) {
      // The proxy reads `Authorization: Bearer` before `x-api-key`; never forward a provider
      // token (authToken, ANTHROPIC_AUTH_TOKEN, credential providers) in place of the TokenLens key.
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
      const runId = getHeader(headers, 'x-tl-run-id') ?? currentRun()?.runId;
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
  const stripBearer = provider === 'anthropic';
  const root = resolveBaseURL(opts.baseURL);
  const overrides = {
    apiKey: resolveApiKey(opts.apiKey),
    baseURL: provider === 'openai' ? `${root}/v1` : root,
    ...(stripBearer ? { authToken: null } : {}),
  };
  const Base = (c.constructor as ClientClass)[UNGUARDED] ?? (c.constructor as ClientClass);
  const Guarded = guardedClass(Base, opts.agent, stripBearer);
  if (typeof c.withOptions !== 'function') {
    return new Guarded({ ...c._options, ...overrides }) as T;
  }
  const copy = c.withOptions(overrides);
  Object.setPrototypeOf(copy, Guarded.prototype);
  return copy as unknown as T;
}

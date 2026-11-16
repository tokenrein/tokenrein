import { envVar } from './env.js';
import { type GuardOptions, resolveApiKey, resolveBaseURL, wrap, wrapDirect } from './clients.js';
import { reporterFor } from './direct.js';
import { currentRun, validateId, validateUser } from './run.js';

export { flush } from './direct.js';

function direct(options: GuardOptions): boolean {
  const mode = options.mode ?? (envVar('MODE') as GuardOptions['mode']) ?? 'direct';
  if (mode !== 'direct' && mode !== 'proxy') throw new TypeError("mode must be 'direct' or 'proxy'");
  return mode === 'direct';
}

function directOptions(options: GuardOptions, defaultProvider: string) {
  return {
    tokenreinURL: resolveBaseURL(options.baseURL),
    apiKey: resolveApiKey(options.apiKey),
    agent: options.agent,
    user: options.user,
    defaultProvider,
    check: options.check ?? true,
  };
}

export { run, currentRun, Run, type RunOptions } from './run.js';
export type { GuardOptions } from './clients.js';

/**
 * Return a copy of an `OpenAI` client guarded by Tokenrein. Works with any provider the
 * `openai` SDK reaches through `baseURL` (OpenAI, Azure, Gemini, Groq, Mistral, DeepSeek, xAI,
 * OpenRouter, Together, Ollama, vLLM, ...).
 *
 * ```ts
 * const client = guard.openai(new OpenAI(), { agent: 'support-bot' });
 * ```
 */
export function openai<T>(client: T, options: GuardOptions = {}): T {
  return direct(options) ? wrapDirect(client, directOptions(options, 'openai'), 'openai') : wrap(client, options, 'openai');
}

/** Return a copy of an `Anthropic` client guarded by Tokenrein. Options as {@link openai}. */
export function anthropic<T>(client: T, options: GuardOptions = {}): T {
  return direct(options)
    ? wrapDirect(client, directOptions(options, 'anthropic'), 'anthropic')
    : wrap(client, options, 'anthropic');
}

export interface LogOptions {
  /**
   * Text for the step (a query, a tool's input or output). Kept only if the project has content
   * capture on, redacted and cut to 2,000 characters; otherwise only its size.
   */
  content?: unknown;
  level?: 'debug' | 'info' | 'warn' | 'error';
  /** `'error'` is implied by `level: 'error'`. */
  status?: 'ok' | 'error';
  durationMs?: number;
  /** Up to 20 small values (numbers, booleans, short strings), always kept. No prompts or secrets. */
  attrs?: Record<string, unknown>;
  kind?: 'log' | 'tool_call' | 'tool_result' | 'prompt' | 'subagent';
  /** Pairs a `tool_call` with its `tool_result`. */
  callId?: string;
  /** Default to the current run's (or `"default"`). */
  agent?: string;
  user?: string;
  apiKey?: string;
  baseURL?: string;
}

const LEVELS = ['debug', 'info', 'warn', 'error'];
const KINDS = ['log', 'tool_call', 'tool_result', 'prompt', 'subagent'];

/**
 * Log one step of the current run to Tokenrein's agent logs (the run's trace and Logs page).
 * Steps cost nothing against budgets; they are queued and sent in the background, and nothing
 * is thrown if Tokenrein can't be reached.
 *
 * ```ts
 * guard.log('retrieve', { attrs: { hits: docs.length } });
 * ```
 */
export function log(name: string, options: LogOptions = {}): void {
  if (typeof name !== 'string' || !name.trim()) throw new TypeError('name must be a non-empty string');
  const level = options.level ?? 'info';
  const kind = options.kind ?? 'log';
  if (!LEVELS.includes(level)) throw new TypeError(`level must be one of ${LEVELS.join(', ')}`);
  if (!KINDS.includes(kind)) throw new TypeError(`kind must be one of ${KINDS.join(', ')}`);
  if (options.status !== undefined && options.status !== 'ok' && options.status !== 'error') {
    throw new TypeError("status must be 'ok' or 'error'");
  }
  const r = currentRun();
  const body: Record<string, unknown> = { name, level, kind };
  if (r) {
    body.run_id = r.runId;
    if (r.parentRunId) body.parent_run_id = r.parentRunId;
  }
  const agent = options.agent ?? r?.agent;
  if (agent !== undefined) body.agent = validateId(agent, 'agent');
  const user = options.user ?? r?.user;
  if (user !== undefined) body.user = validateUser(user);
  if (options.status) body.status = options.status;
  if (options.durationMs !== undefined) body.duration_ms = Math.max(0, Math.round(options.durationMs));
  if (options.content !== undefined && options.content !== null) {
    body.content = typeof options.content === 'string' ? options.content : JSON.stringify(options.content);
  }
  if (options.attrs && Object.keys(options.attrs).length) body.attrs = { ...options.attrs };
  if (options.callId !== undefined) body.call_id = String(options.callId);
  reporterFor(resolveBaseURL(options.baseURL), resolveApiKey(options.apiKey)).log(body);
}

/** Passed to a {@link step} callback: add attributes while the step runs. */
export interface StepHandle {
  attrs: Record<string, unknown>;
}

/**
 * Time `fn` and log it as one step, with `status: 'error'` if it throws (or its promise
 * rejects). Returns what `fn` returns.
 *
 * ```ts
 * const docs = await guard.step('retrieve', async (s) => {
 *   const found = await search(query);
 *   s.attrs.hits = found.length;
 *   return found;
 * });
 * ```
 */
export function step<T>(name: string, fn: (s: StepHandle) => T, options: LogOptions = {}): T {
  const handle: StepHandle = { attrs: { ...(options.attrs ?? {}) } };
  const start = performance.now();
  const done = (error?: unknown) => {
    const failed = error !== undefined;
    if (failed && error instanceof Error) handle.attrs.error ??= error.name;
    log(name, {
      ...options,
      attrs: handle.attrs,
      durationMs: performance.now() - start,
      status: options.status ?? (failed ? 'error' : 'ok'),
      level: options.level ?? (failed ? 'error' : 'info'),
    });
  };
  let out: T;
  try {
    out = fn(handle);
  } catch (e) {
    done(e ?? new Error('step failed'));
    throw e;
  }
  if (out instanceof Promise) {
    return out.then(
      (v) => {
        done();
        return v;
      },
      (e) => {
        done(e ?? new Error('step failed'));
        throw e;
      },
    ) as T;
  }
  done();
  return out;
}

import { type GuardOptions, wrap } from './clients.js';

export { run, currentRun, Run, type RunOptions } from './run.js';
export type { GuardOptions } from './clients.js';

/**
 * Return a copy of an `OpenAI` client that talks to the TokenLens proxy.
 *
 * ```ts
 * const client = guard.openai(new OpenAI(), { agent: 'support-bot' });
 * ```
 */
export function openai<T>(client: T, options: GuardOptions = {}): T {
  return wrap(client, options, 'openai');
}

/** Return a copy of an `Anthropic` client that talks to the TokenLens proxy. Options as {@link openai}. */
export function anthropic<T>(client: T, options: GuardOptions = {}): T {
  return wrap(client, options, 'anthropic');
}

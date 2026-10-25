import { envVar } from './env.js';
import { type GuardOptions, resolveApiKey, resolveBaseURL, wrap, wrapDirect } from './clients.js';

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

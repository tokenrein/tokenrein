/** Run scopes: which run (and parent run) the current async context is executing in. */

import { AsyncLocalStorage } from 'node:async_hooks';
import { randomUUID } from 'node:crypto';

import { LOCAL_CAP_EXCEEDED, LocalCapExceeded } from './errors.js';

// Same rule the proxy applies to run ids and agent names.
const ID_RE = /^[A-Za-z0-9._-]{1,128}$/;

const storage = new AsyncLocalStorage<Run>();

export function validateId(value: string, what: string): string {
  if (typeof value !== 'string' || !ID_RE.test(value)) {
    throw new TypeError(`${what} must be 1-128 letters, digits, '.', '_' or '-' (got ${JSON.stringify(value)})`);
  }
  return value;
}

export function newRunId(): string {
  return `run_${randomUUID().replaceAll('-', '')}`;
}

export interface RunOptions {
  /** Run id (1-128 of `A-Z a-z 0-9 . _ -`). Generated when omitted. */
  runId?: string;
  /** Budget for this run (`x-tl-run-budget-usd`), fixed by the proxy on first use. */
  budgetUsd?: number;
  /** Agent name for clients wrapped without `agent`. Not inherited by child runs. */
  agent?: string;
  /**
   * Person or service the run works for (`x-tl-user`), for spend reports by user. Inherited by
   * child runs; a client's own `user` takes precedence.
   */
  user?: string;
  /**
   * Local fallback cap: at most this many requests in this run (including its child runs),
   * counted in this process and enforced even if the proxy is unreachable. Exceeding it
   * throws {@link LocalCapExceeded} before the request is sent. SDK retries are not counted.
   */
  maxCalls?: number;
}

/** One agent run. Created by `guard.run`; read it with `currentRun()`. */
export class Run {
  readonly runId: string;
  readonly budgetUsd: number | undefined;
  readonly agent: string | undefined;
  readonly user: string | undefined;
  readonly maxCalls: number | undefined;
  readonly parent: Run | undefined;
  #calls = 0;

  constructor(options: RunOptions = {}, parent?: Run) {
    this.runId = options.runId !== undefined ? validateId(options.runId, 'runId') : newRunId();
    const { budgetUsd, maxCalls } = options;
    if (budgetUsd !== undefined && (typeof budgetUsd !== 'number' || !Number.isFinite(budgetUsd) || budgetUsd <= 0)) {
      throw new RangeError('budgetUsd must be a positive number of US dollars');
    }
    if (maxCalls !== undefined && (!Number.isInteger(maxCalls) || maxCalls < 1)) {
      throw new RangeError('maxCalls must be a positive integer');
    }
    this.budgetUsd = budgetUsd;
    this.agent = options.agent !== undefined ? validateId(options.agent, 'agent') : undefined;
    this.user = options.user !== undefined ? validateId(options.user, 'user') : parent?.user;
    this.maxCalls = maxCalls;
    this.parent = parent;
    if (parent && parent.runId === this.runId) {
      throw new RangeError('a run cannot be its own parent');
    }
  }

  get parentRunId(): string | undefined {
    return this.parent?.runId;
  }

  /** Requests made in this run and its child runs (SDK retries are not counted). */
  get calls(): number {
    return this.#calls;
  }

  /** Count one call against this run and every ancestor, or throw if a cap is hit. */
  recordCall(): void {
    const chain: Run[] = [];
    for (let r: Run | undefined = this; r; r = r.parent) chain.push(r);
    // Check every cap first so a refused call is not counted anywhere.
    for (const r of chain) {
      if (r.maxCalls !== undefined && r.#calls >= r.maxCalls) {
        throw new LocalCapExceeded(
          `TokenLens local cap: run ${r.runId} reached maxCalls=${r.maxCalls}; request not sent`,
          { runId: r.runId, errorType: LOCAL_CAP_EXCEEDED },
        );
      }
    }
    for (const r of chain) r.#calls += 1;
  }

  headers(): Record<string, string> {
    const h: Record<string, string> = { 'x-tl-run-id': this.runId };
    if (this.budgetUsd !== undefined) h['x-tl-run-budget-usd'] = String(this.budgetUsd);
    if (this.parent) h['x-tl-parent-run-id'] = this.parent.runId;
    return h;
  }
}

/** The innermost active run in this async context, or `undefined`. */
export function currentRun(): Run | undefined {
  return storage.getStore();
}

/**
 * Run `fn` inside a new run. Every request made by a guarded client inside `fn` (including
 * in awaited promises, timers and child tasks it starts) carries the run's headers. Runs
 * nest: a run started inside another sends `x-tl-parent-run-id`.
 *
 * Returns whatever `fn` returns (a promise if `fn` is async).
 */
export function run<T>(fn: (run: Run) => T): T;
export function run<T>(options: RunOptions | string | undefined, fn: (run: Run) => T): T;
export function run<T>(a: RunOptions | string | undefined | ((run: Run) => T), b?: (run: Run) => T): T {
  const fn = typeof a === 'function' ? a : b;
  if (typeof fn !== 'function') throw new TypeError('guard.run needs a callback');
  const options: RunOptions = typeof a === 'string' ? { runId: a } : typeof a === 'function' ? {} : (a ?? {});
  const r = new Run(options, storage.getStore());
  return storage.run(r, fn, r);
}

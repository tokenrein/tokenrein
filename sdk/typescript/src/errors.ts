/** Errors raised when the TokenLens proxy (or the local fallback cap) stops a call. */

export const BUDGET_EXCEEDED = 'tokenlens_budget_exceeded';
export const RUN_PAUSED = 'tokenlens_run_paused';
export const RUN_KILLED = 'tokenlens_run_killed';
export const LOCAL_CAP_EXCEEDED = 'tokenlens_local_cap_exceeded';

/** Set on SDK errors that TokenLens re-labels; points at the TokenLens class they belong to. */
const KIND = Symbol.for('tokenlens.errorKind');

/**
 * Base class for every TokenLens error.
 *
 * Errors from the proxy are the provider SDK's own error objects (e.g. `OpenAI.APIError`,
 * `Anthropic.PermissionDeniedError`) re-labelled as TokenLens errors, so both
 * `err instanceof BudgetExceeded` and existing `err instanceof OpenAI.APIError` checks work.
 */
export class TokenLensError extends Error {
  /** The run the proxy evaluated (response header `x-tl-run-id`), if known. */
  runId: string | undefined;
  /** The proxy's error type, e.g. `tokenlens_budget_exceeded`. */
  errorType: string;
  /** HTTP status from the proxy; `undefined` for local errors. */
  status: number | undefined;

  constructor(message: string, opts: { runId?: string; errorType: string; status?: number }) {
    super(message);
    this.name = new.target.name;
    this.runId = opts.runId;
    this.errorType = opts.errorType;
    this.status = opts.status;
  }

  static [Symbol.hasInstance](value: unknown): boolean {
    if (Function.prototype[Symbol.hasInstance].call(this, value)) return true;
    const kind = (value as { [KIND]?: Function } | null)?.[KIND];
    return typeof kind === 'function' && (kind === this || kind.prototype instanceof this);
  }
}

/** HTTP 402: the call would put the run (or an ancestor, agent, project or org) over budget. */
export class BudgetExceeded extends TokenLensError {}

/** HTTP 403: the run or one of its ancestors was paused or killed. */
export class RunStopped extends TokenLensError {}

/** HTTP 403 `tokenlens_run_paused`: resume the run from the dashboard or admin API. */
export class RunPaused extends RunStopped {}

/** HTTP 403 `tokenlens_run_killed`: the run is blocked permanently. */
export class RunKilled extends RunStopped {}

/** The in-process `maxCalls` cap of a run was reached; the request was not sent. */
export class LocalCapExceeded extends BudgetExceeded {}

const BY_STATUS_AND_TYPE: Record<string, typeof TokenLensError> = {
  [`402 ${BUDGET_EXCEEDED}`]: BudgetExceeded,
  [`403 ${RUN_PAUSED}`]: RunPaused,
  [`403 ${RUN_KILLED}`]: RunKilled,
};

/**
 * `[type, message]` from an OpenAI-shaped `{"error": {...}}` or Anthropic-shaped
 * `{"type": "error", "error": {...}}` body.
 */
export function parseError(body: unknown): [string | undefined, string | undefined] {
  const err = (body as { error?: unknown } | null | undefined)?.error;
  if (!err || typeof err !== 'object') return [undefined, undefined];
  const { type, message } = err as { type?: unknown; message?: unknown };
  return [typeof type === 'string' ? type : undefined, typeof message === 'string' ? message : undefined];
}

/**
 * Re-label the SDK's error for a proxy response as a TokenLens error, in place.
 * Returns `sdkError` unchanged when the response is not a TokenLens 402/403.
 */
export function relabel<E extends Error>(
  sdkError: E,
  status: number,
  body: unknown,
  runId: string | undefined,
): E {
  const [type, message] = parseError(body);
  const cls = type && BY_STATUS_AND_TYPE[`${status} ${type}`];
  if (!cls || !type) return sdkError;
  const firstLine = `${sdkError.name}: ${sdkError.message}`;
  Object.defineProperty(sdkError, KIND, { value: cls });
  const fields: Partial<TokenLensError> = {
    name: cls.name,
    message: message ?? sdkError.message,
    runId,
    errorType: type,
    status,
  };
  Object.assign(sdkError, fields);
  if (typeof sdkError.stack === 'string' && sdkError.stack.startsWith(firstLine)) {
    sdkError.stack = `${sdkError.name}: ${sdkError.message}` + sdkError.stack.slice(firstLine.length);
  }
  return sdkError;
}

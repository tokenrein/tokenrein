"""Exceptions raised when the Tokenrein proxy (or the local fallback cap) stops a call."""

from __future__ import annotations

from typing import Any, Dict, Optional, Tuple, Type

BUDGET_EXCEEDED = "tokenrein_budget_exceeded"
RUN_PAUSED = "tokenrein_run_paused"
RUN_KILLED = "tokenrein_run_killed"
LOCAL_CAP_EXCEEDED = "tokenrein_local_cap_exceeded"


class TokenreinError(Exception):
    """Base class for every error Tokenrein raises.

    Attributes:
        message: Human-readable reason from the proxy.
        run_id: The run the proxy evaluated (response header ``x-tr-run-id``), if known.
        error_type: The proxy's error type, e.g. ``tokenrein_budget_exceeded``.
        status_code: HTTP status from the proxy, or ``None`` for local errors.
    """

    message: str
    run_id: Optional[str]
    error_type: Optional[str]
    status_code: Optional[int]

    def __init__(
        self,
        message: str,
        *,
        run_id: Optional[str] = None,
        error_type: Optional[str] = None,
        status_code: Optional[int] = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.run_id = run_id
        self.error_type = error_type
        self.status_code = status_code


class BudgetExceeded(TokenreinError):
    """HTTP 402: the call would put the run (or an ancestor, agent, project or org) over budget."""


class RunStopped(TokenreinError):
    """HTTP 403: the run or one of its ancestors was paused or killed."""


class RunPaused(RunStopped):
    """HTTP 403 ``tokenrein_run_paused``: resume the run from the dashboard or admin API."""


class RunKilled(RunStopped):
    """HTTP 403 ``tokenrein_run_killed``: the run is blocked permanently."""


class LocalCapExceeded(BudgetExceeded):
    """The in-process ``max_calls`` cap of a run was reached; the request was not sent."""


_BY_STATUS_AND_TYPE: Dict[Tuple[int, str], Type[TokenreinError]] = {
    (402, BUDGET_EXCEEDED): BudgetExceeded,
    (403, RUN_PAUSED): RunPaused,
    (403, RUN_KILLED): RunKilled,
}

# (Tokenrein class, SDK error class) -> class inheriting from both.
_COMBINED: Dict[Tuple[type, type], type] = {}


def _combined_class(tl_cls: Type[TokenreinError], sdk_cls: type) -> type:
    """A class that is both ``tl_cls`` and the SDK's own status error, so existing
    ``except openai.APIStatusError`` handlers keep working."""
    key = (tl_cls, sdk_cls)
    cls = _COMBINED.get(key)
    if cls is None:

        def __init__(self: Any, message: str, *, response: Any, body: object, run_id: Optional[str], error_type: str) -> None:
            sdk_cls.__init__(self, message, response=response, body=body)
            self.message = message
            self.run_id = run_id
            self.error_type = error_type
            self.status_code = response.status_code

        cls = type(tl_cls.__name__, (tl_cls, sdk_cls), {"__init__": __init__, "__module__": tl_cls.__module__})
        _COMBINED[key] = cls
    return cls


def parse_error(body: object) -> Tuple[Optional[str], Optional[str]]:
    """Return ``(error_type, message)`` from an OpenAI-shaped ``{"error": {...}}`` or
    Anthropic-shaped ``{"type": "error", "error": {...}}`` body."""
    if not isinstance(body, dict):
        return None, None
    err = body.get("error")
    if not isinstance(err, dict):
        return None, None
    kind = err.get("type")
    message = err.get("message")
    return (kind if isinstance(kind, str) else None, message if isinstance(message, str) else None)


def from_response(response: Any, body: object, default: Exception) -> Optional[Exception]:
    """Map a proxy error response to a Tokenrein exception, or ``None`` if it is not one.

    ``default`` is the exception the provider SDK would have raised; the returned
    exception subclasses its type.
    """
    kind, message = parse_error(body)
    if kind is None:
        return None
    tl_cls = _BY_STATUS_AND_TYPE.get((response.status_code, kind))
    if tl_cls is None:
        return None
    run_id = response.headers.get("x-tr-run-id") or response.request.headers.get("x-tr-run-id")
    cls = _combined_class(tl_cls, type(default))
    return cls(
        message or str(default),
        response=response,
        body=getattr(default, "body", body),
        run_id=run_id,
        error_type=kind,
    )

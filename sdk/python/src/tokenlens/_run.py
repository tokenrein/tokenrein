"""Run scopes: which run (and parent run) the current code is executing in."""

from __future__ import annotations

import contextvars
import math
import re
import threading
import uuid
from types import TracebackType
from typing import Optional, Type

from .errors import LOCAL_CAP_EXCEEDED, LocalCapExceeded

# Same rule the proxy applies to run ids and agent names.
_ID_RE = re.compile(r"^[A-Za-z0-9._-]{1,128}$")
# Users may be emails.
_USER_RE = re.compile(r"^[A-Za-z0-9._@+-]{1,128}$")

# Guards call counters; runs may be shared across threads (asyncio.to_thread copies the context).
_calls_lock = threading.Lock()

_current: contextvars.ContextVar[Optional["Run"]] = contextvars.ContextVar("tokenlens_run", default=None)


def validate_user(value: str) -> str:
    if not isinstance(value, str) or not _USER_RE.match(value):
        raise ValueError(f"user must be 1-128 letters, digits, '.', '_', '-', '@' or '+' (got {value!r})")
    return value


def validate_id(value: str, what: str) -> str:
    if not isinstance(value, str) or not _ID_RE.match(value):
        raise ValueError(f"{what} must be 1-128 letters, digits, '.', '_' or '-' (got {value!r})")
    return value


def new_run_id() -> str:
    return f"run_{uuid.uuid4().hex}"


class Run:
    """One agent run. Created by :func:`tokenlens.guard.run`; read it with :func:`current_run`."""

    def __init__(
        self,
        run_id: Optional[str] = None,
        budget_usd: Optional[float] = None,
        *,
        agent: Optional[str] = None,
        max_calls: Optional[int] = None,
        parent: Optional["Run"] = None,
        user: Optional[str] = None,
    ) -> None:
        self.run_id = validate_id(run_id, "run_id") if run_id is not None else new_run_id()
        if budget_usd is not None:
            if isinstance(budget_usd, bool) or not isinstance(budget_usd, (int, float)):
                raise TypeError("budget_usd must be a number")
            if not math.isfinite(budget_usd) or budget_usd <= 0:
                raise ValueError("budget_usd must be a positive number of US dollars")
        self.budget_usd = budget_usd
        self.agent = validate_id(agent, "agent") if agent is not None else None
        # The person or service the run works for; child runs inherit it.
        if user is not None:
            self.user: Optional[str] = validate_user(user)
        else:
            self.user = parent.user if parent is not None else None
        if max_calls is not None and (isinstance(max_calls, bool) or not isinstance(max_calls, int) or max_calls < 1):
            raise ValueError("max_calls must be a positive integer")
        self.max_calls = max_calls
        self.parent = parent
        if parent is not None and parent.run_id == self.run_id:
            raise ValueError("a run cannot be its own parent")
        self._calls = 0

    @property
    def parent_run_id(self) -> Optional[str]:
        return self.parent.run_id if self.parent is not None else None

    @property
    def calls(self) -> int:
        """Requests made in this run and its child runs (SDK retries are not counted)."""
        return self._calls

    def _record_call(self) -> None:
        """Count one call against this run and every ancestor, or raise if a cap is hit.

        Checks every cap first so a refused call is not counted anywhere.
        """
        chain = []
        run: Optional[Run] = self
        while run is not None:
            chain.append(run)
            run = run.parent
        with _calls_lock:
            for r in chain:
                if r.max_calls is not None and r._calls >= r.max_calls:
                    raise LocalCapExceeded(
                        f"TokenLens local cap: run {r.run_id} reached max_calls={r.max_calls}; request not sent",
                        run_id=r.run_id,
                        error_type=LOCAL_CAP_EXCEEDED,
                    )
            for r in chain:
                r._calls += 1

    def headers(self) -> "dict[str, str]":
        h = {"x-tl-run-id": self.run_id}
        if self.budget_usd is not None:
            h["x-tl-run-budget-usd"] = repr(float(self.budget_usd))
        if self.parent is not None:
            h["x-tl-parent-run-id"] = self.parent.run_id
        return h

    def __repr__(self) -> str:
        return f"Run(run_id={self.run_id!r}, parent_run_id={self.parent_run_id!r}, budget_usd={self.budget_usd!r})"


class RunScope:
    """Context manager returned by :func:`tokenlens.guard.run`. Works with ``with`` and ``async with``."""

    def __init__(
        self,
        run_id: Optional[str],
        budget_usd: Optional[float],
        agent: Optional[str],
        max_calls: Optional[int],
        user: Optional[str] = None,
    ) -> None:
        # Validate eagerly so a bad argument fails at the call site.
        Run(run_id, budget_usd, agent=agent, max_calls=max_calls, user=user)
        self._args = (run_id, budget_usd, agent, max_calls, user)
        self._token: Optional[contextvars.Token[Optional[Run]]] = None

    def __enter__(self) -> Run:
        if self._token is not None:
            raise RuntimeError("this guard.run() scope is already active; create a new one")
        run_id, budget_usd, agent, max_calls, user = self._args
        run = Run(run_id, budget_usd, agent=agent, max_calls=max_calls, parent=_current.get(), user=user)
        self._token = _current.set(run)
        return run

    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> None:
        assert self._token is not None
        _current.reset(self._token)
        self._token = None

    async def __aenter__(self) -> Run:
        return self.__enter__()

    async def __aexit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> None:
        self.__exit__(exc_type, exc, tb)


def current_run() -> Optional[Run]:
    """The innermost active run in this context, or ``None``."""
    return _current.get()

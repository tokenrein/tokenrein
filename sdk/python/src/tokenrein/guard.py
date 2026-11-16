"""The Tokenrein SDK entry points.

.. code-block:: python

    from openai import OpenAI
    from tokenrein import guard

    client = guard.openai(OpenAI(), agent="support-bot")
    with guard.run("ticket-4812", budget_usd=5):
        client.chat.completions.create(...)
"""

from __future__ import annotations

import time
from types import TracebackType
from typing import Any, Dict, Optional, Type


from . import _clients, _direct
from ._run import Run, RunScope, current_run, validate_id, validate_user

__all__ = ["openai", "anthropic", "run", "current_run", "Run", "flush", "log", "step", "Step"]

#: Send every queued usage report now (direct mode). Reports are also flushed at exit.
flush = _direct.flush


def _mode(mode: Optional[str]) -> str:
    m = (mode or _clients.env("MODE") or "direct").lower()
    if m not in ("direct", "proxy"):
        raise ValueError("mode must be 'direct' or 'proxy'")
    return m


def openai(
    client: Any = None,
    *,
    agent: Optional[str] = None,
    api_key: Optional[str] = None,
    base_url: Optional[str] = None,
    user: Optional[str] = None,
    mode: Optional[str] = None,
    check: bool = True,
) -> Any:
    """Return a copy of an ``openai.OpenAI`` / ``openai.AsyncOpenAI`` client guarded by Tokenrein.

    Works with any provider the ``openai`` SDK reaches through ``base_url``: OpenAI, Azure,
    Gemini, Groq, Mistral, DeepSeek, xAI, OpenRouter, Together, Ollama, vLLM, ...

    Args:
        client: The client to copy. ``None`` creates an ``openai.OpenAI()``.
        agent: Agent name (per-agent daily budget).
        user: Person or service the agent works for, for spend reports by user (emails are fine).
        api_key: Tokenrein key; defaults to ``$TOKENREIN_API_KEY``.
        base_url: Tokenrein URL; defaults to ``$TOKENREIN_BASE_URL`` or ``http://127.0.0.1:8787``.
        mode: ``"direct"`` (default, or ``$TOKENREIN_MODE``): the client keeps calling its
            provider; Tokenrein checks each call first and receives token usage afterwards,
            never prompts. ``"proxy"``: calls go through the Tokenrein proxy, which checks every
            call before forwarding it (strict).
        check: Direct mode only. Ask Tokenrein before each call (default). ``False`` skips the
            round trip and only stops runs that previous usage reports showed are over a limit.
    """
    key = _clients.resolve_api_key(api_key)
    if _mode(mode) == "direct":
        if client is None:
            import openai as openai_sdk

            client = openai_sdk.OpenAI()
        return _clients.wrap_direct(
            client,
            tokenrein_url=_clients.resolve_base_url(base_url),
            api_key=key,
            agent=agent,
            user=user,
            default_provider="openai",
            check=check,
        )
    url = _clients.resolve_base_url(base_url) + "/v1"
    if client is None:
        import openai as openai_sdk

        client = openai_sdk.OpenAI(api_key=key, base_url=url)
    return _clients.wrap(client, base_url=url, api_key=key, agent=agent, strip_bearer=False, user=user)


def anthropic(
    client: Any = None,
    *,
    agent: Optional[str] = None,
    api_key: Optional[str] = None,
    base_url: Optional[str] = None,
    user: Optional[str] = None,
    mode: Optional[str] = None,
    check: bool = True,
) -> Any:
    """Return a copy of an ``anthropic.Anthropic`` / ``anthropic.AsyncAnthropic`` client guarded
    by Tokenrein. Arguments are the same as :func:`openai`."""
    key = _clients.resolve_api_key(api_key)
    if _mode(mode) == "direct":
        if client is None:
            import anthropic as anthropic_sdk

            client = anthropic_sdk.Anthropic()
        return _clients.wrap_direct(
            client,
            tokenrein_url=_clients.resolve_base_url(base_url),
            api_key=key,
            agent=agent,
            user=user,
            default_provider="anthropic",
            check=check,
        )
    url = _clients.resolve_base_url(base_url)
    if client is None:
        import anthropic as anthropic_sdk

        client = anthropic_sdk.Anthropic(api_key=key, base_url=url)
    return _clients.wrap(client, base_url=url, api_key=key, agent=agent, strip_bearer=True, user=user)


def run(
    run_id: Optional[str] = None,
    budget_usd: Optional[float] = None,
    *,
    agent: Optional[str] = None,
    max_calls: Optional[int] = None,
    user: Optional[str] = None,
) -> RunScope:
    """Scope every request made inside it to one run.

    Usable as ``with guard.run(...) as r:`` or ``async with guard.run(...) as r:``. Scopes nest:
    a run opened inside another sends ``x-tr-parent-run-id`` so its spend also counts against
    the enclosing run. State lives in a ``contextvars.ContextVar``, so concurrent asyncio tasks
    and threads each see their own run.

    Args:
        run_id: Run id (1-128 of ``A-Z a-z 0-9 . _ -``). Generated when ``None``.
        budget_usd: Budget for this run (``x-tr-run-budget-usd``), fixed by the proxy on first use.
        agent: Agent name for clients wrapped without ``agent=``. Not inherited by child runs.
        max_calls: Local fallback cap: at most this many requests in this run (including its
            child runs), counted in this process, enforced even if the proxy is unreachable.
            Exceeding it raises :class:`tokenrein.LocalCapExceeded` before the request is sent.
        user: Person or service the run works for (``x-tr-user``), for spend reports by user.
            Inherited by child runs; a client's own ``user`` takes precedence.
    """
    return RunScope(run_id, budget_usd, agent, max_calls, user)


_LEVELS = ("debug", "info", "warn", "error")
_KINDS = ("log", "tool_call", "tool_result", "prompt", "subagent")


def log(
    name: str,
    *,
    content: Any = None,
    level: str = "info",
    status: Optional[str] = None,
    duration_ms: Optional[float] = None,
    attrs: Optional[Dict[str, Any]] = None,
    kind: str = "log",
    call_id: Optional[str] = None,
    agent: Optional[str] = None,
    user: Optional[str] = None,
    api_key: Optional[str] = None,
    base_url: Optional[str] = None,
) -> None:
    """Log one step of the current run to Tokenrein's agent logs (the run's trace and Logs page).

    Steps cost nothing against budgets. They are queued and sent in the background like usage
    reports; nothing is raised if Tokenrein can't be reached.

    Args:
        name: What happened, e.g. ``"retrieve"`` or ``"tool:search"``.
        content: Text for the step (a query, a tool's input or output). Kept only if the project
            has content capture on, redacted and cut to 2,000 characters; otherwise only its size.
        level: ``"debug"``, ``"info"`` (default), ``"warn"`` or ``"error"``.
        status: ``"ok"`` or ``"error"``; ``level="error"`` implies ``"error"``.
        duration_ms: How long the step took.
        attrs: Up to 20 small values (numbers, booleans, short strings), always kept. Don't put
            prompts or secrets here.
        kind: ``"log"`` (default), ``"tool_call"``, ``"tool_result"``, ``"prompt"`` or ``"subagent"``.
        call_id: Pairs a ``tool_call`` with its ``tool_result``.
        agent, user: Default to the current run's (or ``"default"``).
        api_key, base_url: Default to ``$TOKENREIN_API_KEY`` and ``$TOKENREIN_BASE_URL``.
    """
    if not isinstance(name, str) or not name.strip():
        raise ValueError("name must be a non-empty string")
    if level not in _LEVELS:
        raise ValueError("level must be one of " + ", ".join(_LEVELS))
    if kind not in _KINDS:
        raise ValueError("kind must be one of " + ", ".join(_KINDS))
    if status not in (None, "ok", "error"):
        raise ValueError("status must be 'ok' or 'error'")
    run = current_run()
    body: Dict[str, Any] = {"name": name, "level": level, "kind": kind}
    if run is not None:
        body["run_id"] = run.run_id
        if run.parent_run_id:
            body["parent_run_id"] = run.parent_run_id
    who_agent = agent if agent is not None else (run.agent if run is not None else None)
    if who_agent is not None:
        body["agent"] = validate_id(who_agent, "agent")
    who_user = user if user is not None else (run.user if run is not None else None)
    if who_user is not None:
        body["user"] = validate_user(who_user)
    if status is not None:
        body["status"] = status
    if duration_ms is not None:
        body["duration_ms"] = max(0, int(duration_ms))
    if content is not None:
        body["content"] = content if isinstance(content, str) else repr(content)
    if attrs:
        body["attrs"] = dict(attrs)
    if call_id is not None:
        body["call_id"] = str(call_id)
    reporter = _direct.reporter_for(_clients.resolve_base_url(base_url), _clients.resolve_api_key(api_key))
    reporter.log(body)


class Step:
    """A timed step: logged when the block ends, with ``status="error"`` if it raised.

    .. code-block:: python

        with guard.step("retrieve", attrs={"index": "docs"}) as s:
            docs = search(query)
            s.attrs["hits"] = len(docs)
    """

    def __init__(self, name: str, **kwargs: Any) -> None:
        self.name = name
        self.attrs: Dict[str, Any] = dict(kwargs.pop("attrs", None) or {})
        self.content: Any = kwargs.pop("content", None)
        self._kwargs = kwargs
        self._start: Optional[float] = None

    def __enter__(self) -> "Step":
        self._start = time.monotonic()
        return self

    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> None:
        elapsed = (time.monotonic() - (self._start or time.monotonic())) * 1000
        kwargs = dict(self._kwargs)
        if exc_type is not None:
            kwargs.setdefault("status", "error")
            kwargs.setdefault("level", "error")
            self.attrs.setdefault("error", exc_type.__name__)
        else:
            kwargs.setdefault("status", "ok")
        try:
            log(self.name, content=self.content, duration_ms=elapsed, attrs=self.attrs or None, **kwargs)
        except ValueError:
            if exc_type is None:
                raise

    async def __aenter__(self) -> "Step":
        return self.__enter__()

    async def __aexit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> None:
        self.__exit__(exc_type, exc, tb)


def step(name: str, **kwargs: Any) -> Step:
    """Time a block and log it as one step (``with`` or ``async with``). Arguments as :func:`log`."""
    return Step(name, **kwargs)

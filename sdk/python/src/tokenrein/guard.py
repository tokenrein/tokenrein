"""The Tokenrein SDK entry points.

.. code-block:: python

    from openai import OpenAI
    from tokenrein import guard

    client = guard.openai(OpenAI(), agent="support-bot")
    with guard.run("ticket-4812", budget_usd=5):
        client.chat.completions.create(...)
"""

from __future__ import annotations

from typing import Any, Optional


from . import _clients, _direct
from ._run import Run, RunScope, current_run

__all__ = ["openai", "anthropic", "run", "current_run", "Run", "flush"]

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

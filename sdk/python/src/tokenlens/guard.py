"""The TokenLens SDK entry points.

.. code-block:: python

    from openai import OpenAI
    from tokenlens import guard

    client = guard.openai(OpenAI(), agent="support-bot")
    with guard.run("ticket-4812", budget_usd=5):
        client.chat.completions.create(...)
"""

from __future__ import annotations

from typing import Any, Optional

from . import _clients
from ._run import Run, RunScope, current_run

__all__ = ["openai", "anthropic", "run", "current_run", "Run"]


def openai(
    client: Any = None,
    *,
    agent: Optional[str] = None,
    api_key: Optional[str] = None,
    base_url: Optional[str] = None,
) -> Any:
    """Return a copy of an ``openai.OpenAI`` / ``openai.AsyncOpenAI`` client that talks to TokenLens.

    Args:
        client: The client to copy. ``None`` creates an ``openai.OpenAI()``.
        agent: Agent name sent as ``x-tl-agent`` (per-agent daily budget).
        api_key: TokenLens virtual key; defaults to ``$TOKENLENS_API_KEY``.
        base_url: Proxy URL; defaults to ``$TOKENLENS_BASE_URL`` or ``http://127.0.0.1:8787``.
    """
    key = _clients.resolve_api_key(api_key)
    url = _clients.resolve_base_url(base_url) + "/v1"
    if client is None:
        import openai as openai_sdk

        client = openai_sdk.OpenAI(api_key=key, base_url=url)
    return _clients.wrap(client, base_url=url, api_key=key, agent=agent, strip_bearer=False)


def anthropic(
    client: Any = None,
    *,
    agent: Optional[str] = None,
    api_key: Optional[str] = None,
    base_url: Optional[str] = None,
) -> Any:
    """Return a copy of an ``anthropic.Anthropic`` / ``anthropic.AsyncAnthropic`` client that talks
    to TokenLens. Arguments are the same as :func:`openai`."""
    key = _clients.resolve_api_key(api_key)
    url = _clients.resolve_base_url(base_url)
    if client is None:
        import anthropic as anthropic_sdk

        client = anthropic_sdk.Anthropic(api_key=key, base_url=url)
    return _clients.wrap(client, base_url=url, api_key=key, agent=agent, strip_bearer=True)


def run(
    run_id: Optional[str] = None,
    budget_usd: Optional[float] = None,
    *,
    agent: Optional[str] = None,
    max_calls: Optional[int] = None,
) -> RunScope:
    """Scope every request made inside it to one run.

    Usable as ``with guard.run(...) as r:`` or ``async with guard.run(...) as r:``. Scopes nest:
    a run opened inside another sends ``x-tl-parent-run-id`` so its spend also counts against
    the enclosing run. State lives in a ``contextvars.ContextVar``, so concurrent asyncio tasks
    and threads each see their own run.

    Args:
        run_id: Run id (1-128 of ``A-Z a-z 0-9 . _ -``). Generated when ``None``.
        budget_usd: Budget for this run (``x-tl-run-budget-usd``), fixed by the proxy on first use.
        agent: Agent name for clients wrapped without ``agent=``. Not inherited by child runs.
        max_calls: Local fallback cap: at most this many requests in this run (including its
            child runs), counted in this process, enforced even if the proxy is unreachable.
            Exceeding it raises :class:`tokenlens.LocalCapExceeded` before the request is sent.
    """
    return RunScope(run_id, budget_usd, agent, max_calls)

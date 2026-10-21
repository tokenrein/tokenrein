"""Wrap OpenAI / Anthropic SDK clients for TokenLens.

Two modes:

* **direct** (default): the client keeps calling its provider. Before each call it asks
  TokenLens whether the run may continue; after each response it reports token usage in the
  background. Works with any provider the ``openai`` SDK can reach (Gemini, Groq, Mistral,
  DeepSeek, OpenRouter, Ollama, ...). See :mod:`tokenlens._direct`.
* **proxy**: the client calls the TokenLens proxy, which checks every call before forwarding it.

In proxy mode the wrapped client is a copy of the caller's client (``with_options``) whose class is
swapped for a cached subclass that:

* adds the ``x-tl-*`` headers of the current run to every request it builds, and counts
  the call against any local ``max_calls`` cap;
* turns the proxy's 402/403 responses into :mod:`tokenlens.errors` exceptions that also
  subclass the SDK's own error, so existing ``except openai.APIStatusError`` still works.
"""

from __future__ import annotations

import asyncio
import inspect
import json
import os
import time
from typing import Any, Dict, Optional, TypeVar

from . import _direct, errors
from ._run import _current, validate_id, validate_user

DEFAULT_BASE_URL = "http://127.0.0.1:8787"

C = TypeVar("C")

_GUARDED: Dict[type, type] = {}


def resolve_base_url(base_url: Optional[str]) -> str:
    url = (base_url or os.environ.get("TOKENLENS_BASE_URL") or DEFAULT_BASE_URL).rstrip("/")
    # Accept either the proxy root or its /v1 URL.
    if url.endswith("/v1"):
        url = url[: -len("/v1")]
    return url


def resolve_api_key(api_key: Optional[str]) -> str:
    key = api_key or os.environ.get("TOKENLENS_API_KEY")
    if not key:
        raise ValueError("TokenLens virtual key missing: pass api_key=... or set TOKENLENS_API_KEY")
    return key


def _tl_headers(agent: Optional[str], user: Optional[str]) -> Dict[str, str]:
    run = _current.get()
    h: Dict[str, str] = {}
    if run is not None:
        h.update(run.headers())
        agent = agent or run.agent
        user = user or run.user
    if agent:
        h["x-tl-agent"] = agent
    if user:
        h["x-tl-user"] = user
    return h


def _guarded_class(base: type, *, strip_bearer: bool) -> type:
    if getattr(base, "_tl_guarded", False):
        return base
    cls = _GUARDED.get(base)
    if cls is not None:
        return cls

    def _build_request(self: Any, options: Any, *args: Any, **kwargs: Any) -> Any:
        request = super(cls, self)._build_request(options, *args, **kwargs)  # type: ignore[misc]
        for name, value in _tl_headers(self._tl_agent, self._tl_user).items():
            # Headers the caller set explicitly (extra_headers / default_headers) win.
            if name not in request.headers:
                request.headers[name] = value
        if strip_bearer:
            # The proxy reads 'Authorization: Bearer' before 'x-api-key'; never forward a
            # provider token (e.g. ANTHROPIC_AUTH_TOKEN) in place of the TokenLens key.
            request.headers.pop("authorization", None)
        run = _current.get()
        if run is not None and kwargs.get("retries_taken", 0) == 0:
            # Older SDKs (e.g. openai 1.0) do not pass retries_taken; their retries count too.
            run._record_call()
        return request

    def _make_status_error(self: Any, err_msg: str, *, body: object, response: Any) -> Any:
        default = super(cls, self)._make_status_error(err_msg, body=body, response=response)  # type: ignore[misc]
        return errors.from_response(response, body, default) or default

    def copy(self: Any, *args: Any, **kwargs: Any) -> Any:
        client = super(cls, self).copy(*args, **kwargs)  # type: ignore[misc]
        client._tl_agent = self._tl_agent
        client._tl_user = self._tl_user
        client._tl_source = self._tl_source
        return client

    namespace: Dict[str, Any] = {
        "_tl_guarded": True,
        "_tl_agent": None,
        "_tl_user": None,
        "_tl_source": None,
        "_build_request": _build_request,
        "_make_status_error": _make_status_error,
        "copy": copy,
        "with_options": copy,
        "__module__": base.__module__,
        "__qualname__": base.__qualname__,
    }
    if strip_bearer:
        # Anthropic credential providers mint a Bearer token through httpx auth.
        namespace["custom_auth"] = property(lambda self: None)
    cls = type(base.__name__, (base,), namespace)
    _GUARDED[base] = cls
    return cls


def wrap(
    client: Any,
    *,
    base_url: str,
    api_key: str,
    agent: Optional[str],
    strip_bearer: bool,
    user: Optional[str] = None,
) -> Any:
    if agent is not None:
        validate_id(agent, "agent")
    if user is not None:
        validate_user(user)
    guarded = client.with_options(api_key=api_key, base_url=base_url)
    guarded.__class__ = _guarded_class(type(client), strip_bearer=strip_bearer)
    guarded._tl_agent = agent
    guarded._tl_user = user
    # Copies share the original's HTTP client, and older SDKs (e.g. openai 1.0) close it when
    # the original client is garbage-collected; keep the original alive with the copy.
    guarded._tl_source = getattr(client, "_tl_source", None) or client
    if strip_bearer and hasattr(guarded, "auth_token"):
        guarded.auth_token = None
    return guarded


# --- Direct mode ---------------------------------------------------------------------------

_DIRECT: Dict[type, type] = {}

# Request body fields that bound a call's output tokens, by API.
_MAX_OUTPUT_FIELDS = ("max_tokens", "max_completion_tokens", "max_output_tokens")


def _who(client: Any) -> Dict[str, Any]:
    run = _current.get()
    agent = client._tl_agent or (run.agent if run else None)
    user = client._tl_user or (run.user if run else None)
    who: Dict[str, Any] = {"source": "python-sdk"}
    if run is not None:
        who["run_id"] = run.run_id
        if run.parent is not None:
            who["parent_run_id"] = run.parent.run_id
        if run.budget_usd is not None:
            who["run_budget_usd"] = float(run.budget_usd)
    if agent:
        who["agent"] = agent
    if user:
        who["user"] = user
    return who


def _check_body(client: Any, options: Any) -> Dict[str, Any]:
    body = _who(client)
    data = getattr(options, "json_data", None)
    if isinstance(data, dict):
        if isinstance(data.get("model"), str):
            body["model"] = data["model"]
            body["provider"] = client._tl_provider
        for f in _MAX_OUTPUT_FIELDS:
            if isinstance(data.get(f), int):
                body["max_output_tokens"] = data[f]
                break
    return body


def _include_stream_usage(client: Any, options: Any) -> None:
    """OpenAI-style streams only report usage when asked to."""
    data = getattr(options, "json_data", None)
    url = str(getattr(options, "url", ""))
    if (
        client._tl_provider != "anthropic"
        and isinstance(data, dict)
        and data.get("stream") is True
        and url.endswith("/chat/completions")
        and "stream_options" not in data
    ):
        data["stream_options"] = {"include_usage": True}


def _deny_if_over(client: Any) -> None:
    """With checks off, refuse calls in runs the last reports said are over an enforced limit."""
    run = _current.get()
    rid = run.run_id if run else None
    agent = client._tl_agent or (run.agent if run else None)
    for label in client._tl_reporter.over:
        kind, _, rest = label.partition(":")
        hit = (
            (kind == "run" and rest == rid)
            or (kind == "agent_daily" and agent is not None and rest.endswith("/" + agent))
            or kind in ("project_monthly", "org_monthly")
        )
        if hit:
            raise errors.BudgetExceeded(
                f"TokenLens: the {label} budget is used up",
                run_id=rid,
                error_type=errors.BUDGET_EXCEEDED,
            )


def _report(client: Any, who: Dict[str, Any], model: Optional[str], usage: Optional[Dict[str, int]],
            started: float, stream: bool) -> None:
    if usage is None:
        return
    event = dict(who)
    event.update(usage)
    event["model"] = model or "unknown"
    event["provider"] = client._tl_provider
    event["duration_ms"] = int((time.monotonic() - started) * 1000)
    event["stream"] = stream
    client._tl_reporter.report(event)


def _observe_sync(client: Any, response: Any, stream: bool) -> None:
    who, started = _who(client), time.monotonic()
    ctype = response.headers.get("content-type", "")
    if stream or "text/event-stream" in ctype:
        parser = _direct.StreamUsage()
        original = response.iter_bytes

        def iter_bytes(*a: Any, **kw: Any) -> Any:
            return _direct.tee_sync(
                original(*a, **kw), parser,
                lambda: _report(client, who, parser.model, parser.usage, started, True),
            )

        response.iter_bytes = iter_bytes
    elif "json" in ctype:
        try:
            body = json.loads(response.read())
        except (ValueError, OSError):
            return
        _report(client, who, body.get("model"), _direct.usage_from(body), started, False)


async def _observe_async(client: Any, response: Any, stream: bool) -> None:
    who, started = _who(client), time.monotonic()
    ctype = response.headers.get("content-type", "")
    if stream or "text/event-stream" in ctype:
        parser = _direct.StreamUsage()
        original = response.aiter_bytes

        def aiter_bytes(*a: Any, **kw: Any) -> Any:
            return _direct.tee_async(
                original(*a, **kw), parser,
                lambda: _report(client, who, parser.model, parser.usage, started, True),
            )

        response.aiter_bytes = aiter_bytes
    elif "json" in ctype:
        try:
            body = json.loads(await response.aread())
        except (ValueError, OSError):
            return
        _report(client, who, body.get("model"), _direct.usage_from(body), started, False)


def _direct_class(base: type) -> type:
    if getattr(base, "_tl_direct", False):
        return base
    cls = _DIRECT.get(base)
    if cls is not None:
        return cls
    is_async = inspect.iscoroutinefunction(getattr(base, "_process_response", None))

    def precheck_body(self: Any, options: Any) -> Optional[Dict[str, Any]]:
        _include_stream_usage(self, options)
        if not self._tl_check:
            _deny_if_over(self)
            return None
        return _check_body(self, options)

    def deny(v: Optional[Dict[str, Any]]) -> None:
        if v is not None and not v.get("allowed", True):
            run = _current.get()
            raise _direct.verdict_error(v, run.run_id if run else None)

    if is_async:

        async def _prepare_options(self: Any, options: Any) -> Any:
            # Older SDKs (openai 1.0) mutate in place and return None.
            out = await super(cls, self)._prepare_options(options)  # type: ignore[misc]
            options = out if out is not None else options
            body = precheck_body(self, options)
            if body is not None:
                _, v = await asyncio.to_thread(self._tl_reporter.check, body)
                deny(v)
            return options

        async def _process_response(self: Any, *args: Any, **kwargs: Any) -> Any:
            response = kwargs.get("response")
            if response is not None:
                await _observe_async(self, response, bool(kwargs.get("stream")))
            return await super(cls, self)._process_response(*args, **kwargs)  # type: ignore[misc]

    else:

        def _prepare_options(self: Any, options: Any) -> Any:
            # Older SDKs (openai 1.0) mutate in place and return None.
            out = super(cls, self)._prepare_options(options)  # type: ignore[misc]
            options = out if out is not None else options
            body = precheck_body(self, options)
            if body is not None:
                _, v = self._tl_reporter.check(body)
                deny(v)
            return options

        def _process_response(self: Any, *args: Any, **kwargs: Any) -> Any:
            response = kwargs.get("response")
            if response is not None:
                _observe_sync(self, response, bool(kwargs.get("stream")))
            return super(cls, self)._process_response(*args, **kwargs)  # type: ignore[misc]

    def _build_request(self: Any, options: Any, *args: Any, **kwargs: Any) -> Any:
        request = super(cls, self)._build_request(options, *args, **kwargs)  # type: ignore[misc]
        run = _current.get()
        if run is not None and kwargs.get("retries_taken", 0) == 0:
            run._record_call()
        return request

    def copy(self: Any, *args: Any, **kwargs: Any) -> Any:
        client = super(cls, self).copy(*args, **kwargs)  # type: ignore[misc]
        for attr in ("_tl_agent", "_tl_user", "_tl_reporter", "_tl_provider", "_tl_check", "_tl_source"):
            setattr(client, attr, getattr(self, attr))
        return client

    cls = type(
        base.__name__,
        (base,),
        {
            "_tl_direct": True,
            "_tl_agent": None,
            "_tl_user": None,
            "_tl_reporter": None,
            "_tl_provider": "openai",
            "_tl_check": True,
            "_tl_source": None,
            "_prepare_options": _prepare_options,
            "_process_response": _process_response,
            "_build_request": _build_request,
            "copy": copy,
            "with_options": copy,
            "__module__": base.__module__,
            "__qualname__": base.__qualname__,
        },
    )
    _DIRECT[base] = cls
    return cls


def wrap_direct(
    client: Any,
    *,
    tokenlens_url: str,
    api_key: str,
    agent: Optional[str],
    user: Optional[str],
    default_provider: str,
    check: bool,
) -> Any:
    if agent is not None:
        validate_id(agent, "agent")
    if user is not None:
        validate_user(user)
    base = type(client)
    base = getattr(base, "__mro__", (base,))[1] if getattr(base, "_tl_direct", False) else base
    guarded = client.with_options()
    guarded.__class__ = _direct_class(base)
    guarded._tl_agent = agent
    guarded._tl_user = user
    guarded._tl_reporter = _direct.reporter_for(tokenlens_url, api_key)
    guarded._tl_provider = _direct.provider_for(str(client.base_url), default_provider)
    guarded._tl_check = check
    guarded._tl_source = getattr(client, "_tl_source", None) or client
    return guarded

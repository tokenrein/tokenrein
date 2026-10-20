"""Point OpenAI / Anthropic SDK clients at the TokenLens proxy.

The wrapped client is a copy of the caller's client (``with_options``) whose class is
swapped for a cached subclass that:

* adds the ``x-tl-*`` headers of the current run to every request it builds, and counts
  the call against any local ``max_calls`` cap;
* turns the proxy's 402/403 responses into :mod:`tokenlens.errors` exceptions that also
  subclass the SDK's own error, so existing ``except openai.APIStatusError`` still works.
"""

from __future__ import annotations

import os
from typing import Any, Dict, Optional, TypeVar

from . import errors
from ._run import _current, validate_id

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


def _tl_headers(agent: Optional[str]) -> Dict[str, str]:
    run = _current.get()
    h: Dict[str, str] = {}
    if run is not None:
        h.update(run.headers())
        agent = agent or run.agent
    if agent:
        h["x-tl-agent"] = agent
    return h


def _guarded_class(base: type, *, strip_bearer: bool) -> type:
    if getattr(base, "_tl_guarded", False):
        return base
    cls = _GUARDED.get(base)
    if cls is not None:
        return cls

    def _build_request(self: Any, options: Any, *args: Any, **kwargs: Any) -> Any:
        request = super(cls, self)._build_request(options, *args, **kwargs)  # type: ignore[misc]
        for name, value in _tl_headers(self._tl_agent).items():
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
        client._tl_source = self._tl_source
        return client

    namespace: Dict[str, Any] = {
        "_tl_guarded": True,
        "_tl_agent": None,
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


def wrap(client: Any, *, base_url: str, api_key: str, agent: Optional[str], strip_bearer: bool) -> Any:
    if agent is not None:
        validate_id(agent, "agent")
    guarded = client.with_options(api_key=api_key, base_url=base_url)
    guarded.__class__ = _guarded_class(type(client), strip_bearer=strip_bearer)
    guarded._tl_agent = agent
    # Copies share the original's HTTP client, and older SDKs (e.g. openai 1.0) close it when
    # the original client is garbage-collected; keep the original alive with the copy.
    guarded._tl_source = getattr(client, "_tl_source", None) or client
    if strip_bearer and hasattr(guarded, "auth_token"):
        guarded.auth_token = None
    return guarded

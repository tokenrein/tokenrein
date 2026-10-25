"""Direct mode: the wrapped client calls the provider itself; Tokenrein gets usage only.

Before each call the SDK asks Tokenrein whether the run may continue (``POST /v1/check``); after
each response it reads the token usage (also from streams) and reports it in the background
(``POST /v1/usage``). Prompts and responses never leave the process. Network problems with
Tokenrein never break a call: checks fail open and reports are dropped after a few tries.
"""

from __future__ import annotations

import atexit
import json
import queue
import threading
import time
import urllib.error
import urllib.request
from typing import Any, Callable, Dict, Iterator, List, Optional, Tuple
from urllib.parse import urlparse

from . import errors

_HOST_PROVIDERS = {
    "api.openai.com": "openai",
    "api.anthropic.com": "anthropic",
    "generativelanguage.googleapis.com": "google",
    "api.groq.com": "groq",
    "api.mistral.ai": "mistral",
    "api.deepseek.com": "deepseek",
    "api.x.ai": "xai",
    "openrouter.ai": "openrouter",
    "api.together.xyz": "together",
    "api.fireworks.ai": "fireworks",
    "api.cohere.ai": "cohere",
    "api.cohere.com": "cohere",
}


def provider_for(base_url: str, default: str) -> str:
    """Name the provider from the client's base URL (OpenAI-compatible clients reach many)."""
    host = (urlparse(str(base_url)).hostname or "").lower()
    if host in _HOST_PROVIDERS:
        return _HOST_PROVIDERS[host]
    if host.endswith(".openai.azure.com"):
        return "openai"
    port = urlparse(str(base_url)).port
    if host in ("localhost", "127.0.0.1", "::1"):
        # Well-known local model servers are free; any other local address may be a gateway to
        # paid models, so it is priced by model name.
        return {11434: "ollama", 1234: "lmstudio", 8000: "vllm"}.get(port or 0, default)
    return default


def usage_from(obj: Dict[str, Any]) -> Optional[Dict[str, int]]:
    """Token counts from an OpenAI- or Anthropic-shaped ``usage`` object."""
    u = obj.get("usage") if isinstance(obj, dict) else None
    if not isinstance(u, dict):
        return None

    def n(*keys: str) -> int:
        cur: Any = u
        for k in keys:
            cur = cur.get(k) if isinstance(cur, dict) else None
        return int(cur) if isinstance(cur, (int, float)) else 0

    if "prompt_tokens" in u:  # OpenAI Chat Completions (and compatible providers)
        cached = n("prompt_tokens_details", "cached_tokens")
        return {
            "input_tokens": max(0, n("prompt_tokens") - cached),
            "output_tokens": n("completion_tokens"),
            "cache_read_tokens": cached,
            "cache_write_tokens": 0,
        }
    if "input_tokens_details" in u or "output_tokens_details" in u:  # OpenAI Responses
        cached = n("input_tokens_details", "cached_tokens")
        return {
            "input_tokens": max(0, n("input_tokens") - cached),
            "output_tokens": n("output_tokens"),
            "cache_read_tokens": cached,
            "cache_write_tokens": 0,
        }
    if "input_tokens" in u or "output_tokens" in u:  # Anthropic Messages
        return {
            "input_tokens": n("input_tokens"),
            "output_tokens": n("output_tokens"),
            "cache_read_tokens": n("cache_read_input_tokens"),
            "cache_write_tokens": n("cache_creation_input_tokens"),
        }
    return None


class StreamUsage:
    """Accumulates usage from server-sent events as they pass by."""

    def __init__(self) -> None:
        self._buf = b""
        self.model: Optional[str] = None
        self.usage: Optional[Dict[str, int]] = None

    def feed(self, chunk: bytes) -> None:
        self._buf += chunk
        while b"\n" in self._buf:
            line, self._buf = self._buf.split(b"\n", 1)
            self._line(line.strip())

    def _line(self, line: bytes) -> None:
        if not line.startswith(b"data:"):
            return
        data = line[5:].strip()
        if not data or data == b"[DONE]":
            return
        try:
            ev = json.loads(data)
        except ValueError:
            return
        if not isinstance(ev, dict):
            return
        kind = ev.get("type")
        if kind == "message_start":  # Anthropic: input and cache tokens up front
            msg = ev.get("message") or {}
            self.model = msg.get("model") or self.model
            self.usage = usage_from(msg) or self.usage
        elif kind == "message_delta":  # Anthropic: cumulative output tokens
            out = (ev.get("usage") or {}).get("output_tokens")
            if isinstance(out, int):
                self.usage = dict(self.usage or usage_from({"usage": {}}) or {})
                self.usage.setdefault("input_tokens", 0)
                self.usage.setdefault("cache_read_tokens", 0)
                self.usage.setdefault("cache_write_tokens", 0)
                self.usage["output_tokens"] = out
        elif kind == "response.completed":  # OpenAI Responses
            resp = ev.get("response") or {}
            self.model = resp.get("model") or self.model
            self.usage = usage_from(resp) or self.usage
        else:  # OpenAI Chat Completions chunk; usage arrives on the last one
            self.model = ev.get("model") or self.model
            self.usage = usage_from(ev) or self.usage


def tee_sync(it: Iterator[bytes], parser: StreamUsage, done: Callable[[], None]) -> Iterator[bytes]:
    try:
        for chunk in it:
            parser.feed(chunk)
            yield chunk
    finally:
        done()


async def tee_async(it: Any, parser: StreamUsage, done: Callable[[], None]) -> Any:
    try:
        async for chunk in it:
            parser.feed(chunk)
            yield chunk
    finally:
        done()


class Reporter:
    """Talks to Tokenrein: budget checks (blocking, short timeout) and usage reports (queued,
    sent from a background thread in batches)."""

    BATCH = 100

    def __init__(self, base_url: str, api_key: str, check_timeout: float = 2.0) -> None:
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.check_timeout = check_timeout
        self._q: "queue.Queue[Optional[Dict[str, Any]]]" = queue.Queue(maxsize=10_000)
        self._lock = threading.Lock()
        self._thread: Optional[threading.Thread] = None
        self._pending = 0
        self._idle = threading.Condition(self._lock)
        # Enforced limits the last reports said are used up: label -> time noted.
        self.over: Dict[str, float] = {}

    def _post(self, path: str, body: Dict[str, Any], timeout: float) -> Dict[str, Any]:
        req = urllib.request.Request(
            self.base_url + path,
            data=json.dumps(body).encode(),
            headers={"Authorization": "Bearer " + self.api_key, "content-type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read() or b"{}")

    # --- checks ---

    def check(self, body: Dict[str, Any]) -> Tuple[bool, Optional[Dict[str, Any]]]:
        """(allowed, verdict). Fails open when Tokenrein can't be reached."""
        try:
            v = self._post("/v1/check", body, self.check_timeout)
        except urllib.error.HTTPError as e:
            if e.code == 401:
                raise errors.TokenreinError(
                    "Tokenrein rejected the API key (TOKENREIN_API_KEY)",
                    error_type="tokenrein_unauthorized",
                    status_code=401,
                ) from None
            return True, None
        except (OSError, ValueError):
            return True, None
        return bool(v.get("allowed", True)), v

    # --- reports ---

    def report(self, event: Dict[str, Any]) -> None:
        with self._lock:
            if self._thread is None or not self._thread.is_alive():
                self._thread = threading.Thread(target=self._run, name="tokenrein-reporter", daemon=True)
                self._thread.start()
            self._pending += 1
        try:
            self._q.put_nowait(event)
        except queue.Full:
            with self._lock:
                self._pending -= 1
                self._idle.notify_all()

    def flush(self, timeout: float = 5.0) -> bool:
        """Wait until queued reports are sent (or dropped). True if everything went out."""
        deadline = time.monotonic() + timeout
        with self._lock:
            while self._pending:
                left = deadline - time.monotonic()
                if left <= 0:
                    return False
                self._idle.wait(left)
        return True

    def _run(self) -> None:
        while True:
            first = self._q.get()
            batch: List[Dict[str, Any]] = [first] if first else []
            while len(batch) < self.BATCH:
                try:
                    ev = self._q.get(timeout=0.2)
                except queue.Empty:
                    break
                if ev:
                    batch.append(ev)
            if batch:
                self._send(batch)
            with self._lock:
                self._pending -= len(batch)
                self._idle.notify_all()

    def _send(self, batch: List[Dict[str, Any]]) -> None:
        for attempt in range(3):
            try:
                out = self._post("/v1/usage", {"events": batch}, 10.0)
                now = time.time()
                for label in out.get("over", []):
                    self.over[label] = now
                return
            except urllib.error.HTTPError as e:
                if e.code < 500:
                    return
            except (OSError, ValueError):
                pass
            time.sleep(0.5 * (attempt + 1))


_REPORTERS: Dict[Tuple[str, str], Reporter] = {}
_REPORTERS_LOCK = threading.Lock()


def reporter_for(base_url: str, api_key: str) -> Reporter:
    with _REPORTERS_LOCK:
        key = (base_url.rstrip("/"), api_key)
        r = _REPORTERS.get(key)
        if r is None:
            r = _REPORTERS[key] = Reporter(base_url, api_key)
        return r


def flush(timeout: float = 5.0) -> bool:
    """Send every queued usage report now (call before a short-lived process exits)."""
    with _REPORTERS_LOCK:
        reporters = list(_REPORTERS.values())
    return all(r.flush(timeout) for r in reporters)


atexit.register(flush, 3.0)


def verdict_error(v: Dict[str, Any], run_id: Optional[str]) -> errors.TokenreinError:
    message = v.get("reason") or "Tokenrein refused the call"
    state = v.get("run_state")
    rid = v.get("run_id") or run_id
    if state == "killed":
        return errors.RunKilled(message, run_id=rid, error_type=errors.RUN_KILLED)
    if state == "paused":
        return errors.RunPaused(message, run_id=rid, error_type=errors.RUN_PAUSED)
    return errors.BudgetExceeded(message, run_id=rid, error_type=errors.BUDGET_EXCEEDED)

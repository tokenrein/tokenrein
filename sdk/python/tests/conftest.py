"""A fake Tokenrein proxy that records request headers and can answer with proxy errors."""

from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Dict, List, Optional, Tuple

import pytest

OPENAI_OK = {
    "id": "chatcmpl-1",
    "object": "chat.completion",
    "created": 0,
    "model": "gpt-4o-mini",
    "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
}

ANTHROPIC_OK = {
    "id": "msg_1",
    "type": "message",
    "role": "assistant",
    "model": "claude-haiku-4-5",
    "content": [{"type": "text", "text": "ok"}],
    "stop_reason": "end_turn",
    "stop_sequence": None,
    "usage": {"input_tokens": 1, "output_tokens": 1},
}


def proxy_error(path: str, kind: str, message: str) -> Dict[str, Any]:
    """The body the real proxy returns, in the shape of the API being called."""
    if path.endswith("/messages"):
        return {"type": "error", "error": {"type": kind, "message": message}}
    return {"error": {"message": message, "type": kind, "code": kind, "param": None}}


class FakeProxy:
    def __init__(self) -> None:
        self.requests: List[Dict[str, Any]] = []
        self.delay = 0.0
        # (status, error type, message) returned for every request while set.
        self.error: Optional[Tuple[int, str, str]] = None
        self._lock = threading.Lock()
        fake = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args: Any) -> None:
                pass

            def do_POST(self) -> None:
                length = int(self.headers.get("content-length") or 0)
                body = json.loads(self.rfile.read(length) or b"{}")
                headers = {k.lower(): v for k, v in self.headers.items()}
                with fake._lock:
                    fake.requests.append({"path": self.path, "headers": headers, "body": body})
                if fake.delay:
                    time.sleep(fake.delay)
                run_id = headers.get("x-tr-run-id", "req_generated")
                if fake.error is not None:
                    status, kind, message = fake.error
                    payload = proxy_error(self.path, kind, message)
                else:
                    status = 200
                    payload = ANTHROPIC_OK if self.path.endswith("/messages") else OPENAI_OK
                data = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(data)))
                self.send_header("x-tr-run-id", run_id)
                self.send_header("retry-after-ms", "1")
                self.end_headers()
                self.wfile.write(data)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        self._thread = threading.Thread(target=self.server.serve_forever, args=(0.01,), daemon=True)
        self._thread.start()

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


@pytest.fixture(autouse=True)
def no_pre_rename_env(monkeypatch: pytest.MonkeyPatch):
    """The SDK still reads the variable names used before the rename; keep a developer's own
    settings out of the tests."""
    for name in ("API_KEY", "BASE_URL", "MODE"):
        monkeypatch.delenv("TOKENLENS_" + name, raising=False)  # pre-rename name


@pytest.fixture
def proxy(monkeypatch: pytest.MonkeyPatch):
    fake = FakeProxy()
    monkeypatch.setenv("TOKENREIN_BASE_URL", fake.url)
    monkeypatch.setenv("TOKENREIN_API_KEY", "tl_test_key")
    # These tests exercise proxy mode; direct mode has its own tests.
    monkeypatch.setenv("TOKENREIN_MODE", "proxy")
    monkeypatch.delenv("ANTHROPIC_AUTH_TOKEN", raising=False)
    yield fake
    fake.close()

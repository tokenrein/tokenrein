"""Direct mode: the client calls its provider; TokenLens gets a check before and usage after."""

from __future__ import annotations

import asyncio
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Dict, List, Optional

import anthropic
import openai
import pytest

from tokenlens import BudgetExceeded, RunPaused, guard
from tokenlens import _direct

CHAT = {
    "id": "c1", "object": "chat.completion", "created": 0, "model": "gpt-4o-mini",
    "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
    "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
              "prompt_tokens_details": {"cached_tokens": 2}},
}
MESSAGE = {
    "id": "m1", "type": "message", "role": "assistant", "model": "claude-haiku-5-5",
    "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn", "stop_sequence": None,
    "usage": {"input_tokens": 20, "output_tokens": 7, "cache_read_input_tokens": 100,
              "cache_creation_input_tokens": 3},
}


def sse(events: List[Any]) -> bytes:
    out = b""
    for e in events:
        if isinstance(e, tuple):
            name, data = e
            out += b"event: " + name.encode() + b"\n"
        else:
            data = e
        out += b"data: " + (data if isinstance(data, bytes) else json.dumps(data).encode()) + b"\n\n"
    return out


CHAT_STREAM = sse([
    {"id": "c1", "object": "chat.completion.chunk", "created": 0, "model": "gpt-4o-mini",
     "choices": [{"index": 0, "delta": {"content": "o"}, "finish_reason": None}]},
    {"id": "c1", "object": "chat.completion.chunk", "created": 0, "model": "gpt-4o-mini",
     "choices": [], "usage": {"prompt_tokens": 30, "completion_tokens": 9, "total_tokens": 39}},
    b"[DONE]",
])
MESSAGE_STREAM = sse([
    ("message_start", {"type": "message_start", "message": {
        "id": "m1", "type": "message", "role": "assistant", "model": "claude-haiku-5-5", "content": [],
        "stop_reason": None, "stop_sequence": None,
        "usage": {"input_tokens": 40, "output_tokens": 1, "cache_read_input_tokens": 500,
                  "cache_creation_input_tokens": 0}}}),
    ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
    ("content_block_stop", {"type": "content_block_stop", "index": 0}),
    ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                       "usage": {"output_tokens": 12}}),
    ("message_stop", {"type": "message_stop"}),
])


class Server:
    """Small JSON/SSE server that records requests; `respond(path, body)` decides the reply."""

    def __init__(self, respond: Any) -> None:
        self.requests: List[Dict[str, Any]] = []
        self.respond = respond
        srv = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a: Any) -> None:
                pass

            def do_POST(self) -> None:
                n = int(self.headers.get("content-length") or 0)
                body = json.loads(self.rfile.read(n) or b"{}")
                srv.requests.append({"path": self.path, "headers": {k.lower(): v for k, v in self.headers.items()}, "body": body})
                status, ctype, data = srv.respond(self.path, body)
                self.send_response(status)
                self.send_header("content-type", ctype)
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.httpd.daemon_threads = True
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, args=(0.01,), daemon=True).start()

    def close(self) -> None:
        self.httpd.shutdown()
        self.httpd.server_close()


def provider_reply(path: str, body: Dict[str, Any]):
    streaming = body.get("stream") is True
    if path.endswith("/messages"):
        return (200, "text/event-stream", MESSAGE_STREAM) if streaming else (200, "application/json", json.dumps(MESSAGE).encode())
    return (200, "text/event-stream", CHAT_STREAM) if streaming else (200, "application/json", json.dumps(CHAT).encode())


class FakeTokenLens(Server):
    def __init__(self) -> None:
        self.verdict: Dict[str, Any] = {"allowed": True}
        self.over: List[str] = []
        super().__init__(self._reply)

    def _reply(self, path: str, body: Dict[str, Any]):
        if path == "/v1/check":
            return 200, "application/json", json.dumps(self.verdict).encode()
        if path == "/v1/usage":
            return 200, "application/json", json.dumps({"accepted": len(body["events"]), "over": self.over}).encode()
        return 404, "application/json", b"{}"

    def checks(self) -> List[Dict[str, Any]]:
        return [r["body"] for r in self.requests if r["path"] == "/v1/check"]

    def usage(self) -> List[Dict[str, Any]]:
        guard.flush(5)
        return [e for r in self.requests if r["path"] == "/v1/usage" for e in r["body"]["events"]]


@pytest.fixture
def env(monkeypatch: pytest.MonkeyPatch):
    tl, provider = FakeTokenLens(), Server(provider_reply)
    monkeypatch.setenv("TOKENLENS_BASE_URL", tl.url)
    monkeypatch.setenv("TOKENLENS_API_KEY", "tl_direct_key")
    monkeypatch.delenv("TOKENLENS_MODE", raising=False)
    yield tl, provider
    tl.close()
    provider.close()


MSG = [{"role": "user", "content": "hi"}]


def test_calls_go_to_the_provider_and_usage_to_tokenlens(env):
    tl, provider = env
    client = guard.openai(openai.OpenAI(api_key="sk-provider", base_url=provider.url + "/v1"), agent="writer", user="ana@example.com")
    with guard.run("job-1", budget_usd=2):
        with guard.run("job-1-sub"):
            client.chat.completions.create(model="gpt-4o-mini", messages=MSG, max_tokens=50)
    call = provider.requests[0]
    assert call["headers"]["authorization"] == "Bearer sk-provider"
    assert not any(h.startswith("x-tl-") for h in call["headers"])
    check = tl.checks()[0]
    assert (check["run_id"], check["parent_run_id"], check["agent"], check["user"]) == ("job-1-sub", "job-1", "writer", "ana@example.com")
    assert (check["model"], check["max_output_tokens"]) == ("gpt-4o-mini", 50)
    [ev] = tl.usage()
    assert ev["run_id"] == "job-1-sub" and ev["parent_run_id"] == "job-1"
    assert (ev["input_tokens"], ev["cache_read_tokens"], ev["output_tokens"]) == (8, 2, 5)
    assert ev["model"] == "gpt-4o-mini" and ev["source"] == "python-sdk" and ev["stream"] is False
    assert tl.requests[-1]["headers"]["authorization"] == "Bearer tl_direct_key"


def test_anthropic_stream_usage_is_read_from_events(env):
    tl, provider = env
    client = guard.anthropic(anthropic.Anthropic(api_key="sk-ant", base_url=provider.url), agent="bot")
    with client.messages.stream(model="claude-haiku-5-5", max_tokens=64, messages=MSG) as s:
        text = "".join(s.text_stream)
    assert text == "hi"
    [ev] = tl.usage()
    assert (ev["input_tokens"], ev["cache_read_tokens"], ev["output_tokens"]) == (40, 500, 12)
    assert ev["model"] == "claude-haiku-5-5" and ev["stream"] is True


def test_openai_streams_ask_for_usage(env):
    tl, provider = env
    client = guard.openai(openai.OpenAI(api_key="sk", base_url=provider.url + "/v1"))
    chunks = list(client.chat.completions.create(model="gpt-4o-mini", messages=MSG, stream=True))
    last = chunks[-1].usage  # a plain dict on SDKs older than the usage field
    assert (last["completion_tokens"] if isinstance(last, dict) else last.completion_tokens) == 9
    assert provider.requests[0]["body"]["stream_options"] == {"include_usage": True}
    [ev] = tl.usage()
    assert (ev["input_tokens"], ev["output_tokens"]) == (30, 9)


def test_refused_calls_never_reach_the_provider(env):
    tl, provider = env
    client = guard.openai(openai.OpenAI(api_key="sk", base_url=provider.url + "/v1"))
    tl.verdict = {"allowed": False, "reason": "TokenLens: the run:job-2 budget is used up", "scope": "run:job-2", "run_id": "job-2"}
    with guard.run("job-2"):
        with pytest.raises(BudgetExceeded) as e:
            client.chat.completions.create(model="gpt-4o-mini", messages=MSG)
    assert e.value.run_id == "job-2" and "used up" in e.value.message
    tl.verdict = {"allowed": False, "reason": "paused", "run_state": "paused", "run_id": "job-2"}
    with pytest.raises(RunPaused):
        client.chat.completions.create(model="gpt-4o-mini", messages=MSG)
    assert provider.requests == []


def test_without_checks_reported_overruns_stop_the_run(env):
    tl, provider = env
    client = guard.openai(openai.OpenAI(api_key="sk", base_url=provider.url + "/v1"), check=False)
    tl.over = ["run:job-3"]
    with guard.run("job-3"):
        client.chat.completions.create(model="gpt-4o-mini", messages=MSG)
        tl.usage()  # the report comes back saying run:job-3 is used up
        with pytest.raises(BudgetExceeded):
            client.chat.completions.create(model="gpt-4o-mini", messages=MSG)
    assert tl.checks() == []
    assert len(provider.requests) == 1


def test_tokenlens_down_never_breaks_calls(env, monkeypatch):
    tl, provider = env
    tl.close()
    client = guard.openai(openai.OpenAI(api_key="sk", base_url=provider.url + "/v1"), api_key="tl_other")
    out = client.chat.completions.create(model="gpt-4o-mini", messages=MSG)
    assert out.choices[0].message.content == "ok"


async def test_async_clients(env):
    tl, provider = env
    client = guard.openai(openai.AsyncOpenAI(api_key="sk", base_url=provider.url + "/v1"), agent="a")
    async with guard.run("async-1"):
        await asyncio.gather(*(client.chat.completions.create(model="gpt-4o-mini", messages=MSG) for _ in range(3)))
    events = tl.usage()
    assert len(events) == 3 and {e["run_id"] for e in events} == {"async-1"}
    ant = guard.anthropic(anthropic.AsyncAnthropic(api_key="sk", base_url=provider.url))
    msg = await ant.messages.create(model="claude-haiku-5-5", max_tokens=10, messages=MSG)
    assert msg.content[0].text == "ok"
    assert tl.usage()[-1]["cache_write_tokens"] == 3


def test_provider_names_from_base_urls():
    p = _direct.provider_for
    assert p("https://api.openai.com/v1", "x") == "openai"
    assert p("https://generativelanguage.googleapis.com/v1beta/openai/", "x") == "google"
    assert p("https://api.groq.com/openai/v1", "x") == "groq"
    assert p("https://myco.openai.azure.com/openai", "x") == "openai"
    assert p("http://localhost:11434/v1", "x") == "ollama"
    assert p("http://localhost:4000/v1", "openai") == "openai"
    assert p("https://llm.internal.example", "openai") == "openai"


def test_usage_shapes():
    u = _direct.usage_from
    assert u({"usage": {"input_tokens": 100, "output_tokens": 5, "input_tokens_details": {"cached_tokens": 40}}}) == {
        "input_tokens": 60, "output_tokens": 5, "cache_read_tokens": 40, "cache_write_tokens": 0}
    assert u({"usage": None}) is None
    s = _direct.StreamUsage()
    s.feed(b'data: {"type": "response.completed", "response": {"model": "gpt-5", "usage": {"input_tokens": 9, "output_tokens": 3, "output_tokens_details": {}}}}\n')
    assert s.model == "gpt-5" and s.usage["output_tokens"] == 3

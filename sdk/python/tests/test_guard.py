from __future__ import annotations

import asyncio
import inspect
import re

import anthropic
import openai
import pytest

import tokenrein
from tokenrein import BudgetExceeded, LocalCapExceeded, RunKilled, RunPaused, guard

MSG = [{"role": "user", "content": "hi"}]


def chat(client, **kw):
    return client.chat.completions.create(model="gpt-4o-mini", messages=MSG, **kw)


def message(client, **kw):
    return client.messages.create(model="claude-haiku-4-5", max_tokens=16, messages=MSG, **kw)


def openai_client(**kw):
    return guard.openai(openai.OpenAI(api_key="sk-real-openai-key"), **kw)


def anthropic_client(**kw):
    return guard.anthropic(anthropic.Anthropic(api_key="sk-ant-real"), **kw)


# --- configuration -------------------------------------------------------------------------


def test_openai_points_at_proxy_with_virtual_key(proxy):
    client = openai_client(agent="support-bot")
    assert chat(client).choices[0].message.content == "ok"
    req = proxy.requests[-1]
    assert req["path"] == "/v1/chat/completions"
    assert req["headers"]["authorization"] == "Bearer tl_test_key"
    assert req["headers"]["x-tr-agent"] == "support-bot"
    # Outside a run: no run headers, the proxy makes one run per request.
    assert "x-tr-run-id" not in req["headers"]
    assert "x-tr-parent-run-id" not in req["headers"]


def test_anthropic_points_at_proxy_and_drops_provider_bearer(proxy):
    # An OAuth-token client: newer SDKs refuse a second credential, so the guard must drop it.
    raw = anthropic.Anthropic(auth_token="real-oauth-token")
    client = guard.anthropic(raw, agent="writer")
    assert raw.auth_token == "real-oauth-token", "the caller's client is unchanged"
    assert message(client).content[0].text == "ok"
    req = proxy.requests[-1]
    assert req["path"] == "/v1/messages"
    assert req["headers"]["x-api-key"] == "tl_test_key"
    assert "authorization" not in req["headers"]
    assert req["headers"]["x-tr-agent"] == "writer"


def test_explicit_args_override_env_and_v1_suffix_is_accepted(proxy, monkeypatch):
    monkeypatch.setenv("TOKENREIN_BASE_URL", "http://127.0.0.1:1")  # would fail if used
    client = guard.openai(openai.OpenAI(api_key="x"), api_key="tl_arg_key", base_url=proxy.url + "/v1/")
    chat(client)
    assert proxy.requests[-1]["path"] == "/v1/chat/completions"
    assert proxy.requests[-1]["headers"]["authorization"] == "Bearer tl_arg_key"


def test_client_can_be_created_for_you(proxy):
    chat(guard.openai(agent="a"))
    message(guard.anthropic(agent="b"))
    assert [r["headers"]["x-tr-agent"] for r in proxy.requests] == ["a", "b"]


def test_missing_key_is_a_clear_error(monkeypatch):
    monkeypatch.delenv("TOKENREIN_API_KEY", raising=False)
    with pytest.raises(ValueError, match="TOKENREIN_API_KEY"):
        guard.openai(openai.OpenAI(api_key="x"))


def test_variables_named_before_the_rename_still_work(proxy, monkeypatch):
    monkeypatch.delenv("TOKENREIN_API_KEY")
    monkeypatch.setenv("TOKENLENS_API_KEY", "tl_old_name_key")  # pre-rename name
    with pytest.warns(DeprecationWarning, match="TOKENREIN_API_KEY"):
        client = guard.openai(openai.OpenAI(api_key="x"))
    chat(client)
    assert proxy.requests[-1]["headers"]["authorization"] == "Bearer tl_old_name_key"


def test_invalid_ids_and_budgets_fail_at_the_call_site():
    with pytest.raises(ValueError):
        guard.run("has space")
    with pytest.raises(ValueError):
        guard.run(budget_usd=0)
    with pytest.raises(ValueError):
        guard.run(max_calls=0)
    with pytest.raises(ValueError):
        guard.openai(openai.OpenAI(api_key="x"), agent="bad/agent", api_key="tl")


def test_original_client_is_untouched_and_with_options_keeps_guard(proxy):
    raw = openai.OpenAI(api_key="sk-real-openai-key")
    client = guard.openai(raw, agent="bot")
    assert type(raw) is openai.OpenAI
    assert str(raw.base_url).startswith("https://api.openai.com")
    assert isinstance(client, openai.OpenAI)
    with guard.run("r1"):
        chat(client.with_options(timeout=5))
    assert proxy.requests[-1]["headers"]["x-tr-agent"] == "bot"
    assert proxy.requests[-1]["headers"]["x-tr-run-id"] == "r1"


# --- run headers ---------------------------------------------------------------------------


def test_run_sets_run_id_and_budget(proxy):
    client = openai_client(agent="bot")
    with guard.run("ticket-4812", budget_usd=5) as run:
        assert tokenrein.current_run() is run
        chat(client)
    assert tokenrein.current_run() is None
    h = proxy.requests[-1]["headers"]
    assert h["x-tr-run-id"] == "ticket-4812"
    assert float(h["x-tr-run-budget-usd"]) == 5
    assert h["x-tr-agent"] == "bot"
    assert "x-tr-parent-run-id" not in h


def test_generated_run_id_is_valid_and_stable_within_the_run(proxy):
    client = openai_client()
    with guard.run() as run:
        chat(client)
        chat(client)
    ids = {r["headers"]["x-tr-run-id"] for r in proxy.requests}
    assert ids == {run.run_id}
    assert re.fullmatch(r"run_[0-9a-f]{32}", run.run_id)


def test_nested_runs_form_a_tree(proxy):
    client = openai_client()
    with guard.run("root", budget_usd=10):
        chat(client)
        with guard.run("child", budget_usd=2):
            chat(client)
            with guard.run() as grandchild:
                chat(client)
        chat(client)  # back in root after the child exits
    h = [r["headers"] for r in proxy.requests]
    assert [x["x-tr-run-id"] for x in h] == ["root", "child", grandchild.run_id, "root"]
    assert [x.get("x-tr-parent-run-id") for x in h] == [None, "root", "child", None]
    # Budgets belong to their own run only.
    assert [x.get("x-tr-run-budget-usd") for x in h] == ["10.0", "2.0", None, "10.0"]


def test_run_agent_applies_only_when_client_has_none(proxy):
    named = openai_client(agent="client-agent")
    unnamed = openai_client()
    with guard.run("r", agent="run-agent"):
        chat(named)
        chat(unnamed)
        with guard.run("child"):
            chat(unnamed)
    assert [r["headers"].get("x-tr-agent") for r in proxy.requests] == ["client-agent", "run-agent", None]


def test_explicit_request_headers_win(proxy):
    client = openai_client(agent="bot")
    with guard.run("r"):
        chat(client, extra_headers={"x-tr-run-id": "manual", "x-tr-agent": "manual-agent"})
    h = proxy.requests[-1]["headers"]
    assert (h["x-tr-run-id"], h["x-tr-agent"]) == ("manual", "manual-agent")


def test_run_cannot_be_its_own_parent():
    with guard.run("same"):
        with pytest.raises(ValueError, match="own parent"):
            with guard.run("same"):
                pass


async def test_async_with_and_concurrent_tasks_are_isolated(proxy):
    proxy.delay = 0.02
    oai = guard.openai(openai.AsyncOpenAI(api_key="x"), agent="async-oai")
    ant = guard.anthropic(anthropic.AsyncAnthropic(api_key="x"), agent="async-ant")

    async def worker(i: int) -> None:
        async with guard.run(f"task-{i}", budget_usd=i + 1):
            for step in range(3):
                tag = f"task-{i}/{step}"
                if i % 2:
                    await ant.messages.create(
                        model="claude-haiku-4-5", max_tokens=8, messages=[{"role": "user", "content": tag}]
                    )
                else:
                    await oai.chat.completions.create(
                        model="gpt-4o-mini", messages=[{"role": "user", "content": tag}]
                    )
                async with guard.run(f"task-{i}-sub-{step}"):
                    await oai.chat.completions.create(
                        model="gpt-4o-mini", messages=[{"role": "user", "content": f"{tag}/sub"}]
                    )
                await asyncio.sleep(0)

    await asyncio.gather(*(worker(i) for i in range(12)))

    assert len(proxy.requests) == 12 * 3 * 2
    for req in proxy.requests:
        tag = req["body"]["messages"][0]["content"]
        h = req["headers"]
        task, step = tag.split("/")[:2]
        i = int(task.split("-")[1])
        if tag.endswith("/sub"):
            assert h["x-tr-run-id"] == f"{task}-sub-{step}"
            assert h["x-tr-parent-run-id"] == task
            assert "x-tr-run-budget-usd" not in h
        else:
            assert h["x-tr-run-id"] == task
            assert "x-tr-parent-run-id" not in h
            assert float(h["x-tr-run-budget-usd"]) == i + 1
    assert tokenrein.current_run() is None


async def test_run_is_visible_in_to_thread(proxy):
    client = openai_client()
    async with guard.run("threaded"):
        await asyncio.to_thread(chat, client)
    assert proxy.requests[-1]["headers"]["x-tr-run-id"] == "threaded"


# --- error mapping -------------------------------------------------------------------------


def test_402_openai_shape_maps_to_budget_exceeded(proxy):
    proxy.error = (402, "tokenrein_budget_exceeded", "Tokenrein budget exceeded for run r1")
    client = openai_client()
    with guard.run("r1"):
        with pytest.raises(BudgetExceeded) as ei:
            chat(client)
    e = ei.value
    assert e.run_id == "r1"
    assert e.status_code == 402
    assert e.error_type == "tokenrein_budget_exceeded"
    assert e.message == "Tokenrein budget exceeded for run r1"
    assert isinstance(e, openai.APIStatusError)  # existing SDK handlers still catch it
    assert len(proxy.requests) == 1  # not retried


def test_402_anthropic_shape_maps_to_budget_exceeded(proxy):
    proxy.error = (402, "tokenrein_budget_exceeded", "over budget")
    with pytest.raises(BudgetExceeded) as ei:
        with guard.run("r2"):
            message(anthropic_client())
    assert ei.value.run_id == "r2"
    assert ei.value.message == "over budget"
    assert isinstance(ei.value, anthropic.APIStatusError)


@pytest.mark.parametrize("make,call", [(openai_client, chat), (anthropic_client, message)])
def test_403_paused_and_killed(proxy, make, call):
    client = make()
    proxy.error = (403, "tokenrein_run_paused", "Tokenrein: run r3 is paused")
    with guard.run("r3"):
        with pytest.raises(RunPaused) as paused:
            call(client)
    assert paused.value.run_id == "r3"
    assert paused.value.message == "Tokenrein: run r3 is paused"
    assert isinstance(paused.value, tokenrein.RunStopped)
    assert type(paused.value).__name__ == "RunPaused"
    assert paused.value.status_code == 403

    proxy.error = (403, "tokenrein_run_killed", "Tokenrein: run r3 is killed")
    with guard.run("r3"):
        with pytest.raises(RunKilled) as killed:
            call(client)
    assert killed.value.run_id == "r3"
    assert not isinstance(killed.value, RunPaused)
    sdk = openai if make is openai_client else anthropic
    assert isinstance(killed.value, sdk.PermissionDeniedError)


def test_run_id_falls_back_to_proxy_generated_one(proxy):
    proxy.error = (402, "tokenrein_budget_exceeded", "over")
    with pytest.raises(BudgetExceeded) as ei:
        chat(openai_client())
    assert ei.value.run_id == "req_generated"


def test_other_errors_are_left_to_the_sdk(proxy):
    proxy.error = (403, "permission_error", "nope")
    with pytest.raises(openai.PermissionDeniedError) as ei:
        chat(openai_client())
    assert not isinstance(ei.value, tokenrein.TokenreinError)
    proxy.error = (402, "billing", "pay up")
    with pytest.raises(anthropic.APIStatusError) as ei2:
        message(anthropic_client())
    assert not isinstance(ei2.value, tokenrein.TokenreinError)


def test_streaming_call_raises_mapped_error(proxy):
    proxy.error = (402, "tokenrein_budget_exceeded", "over")
    with pytest.raises(BudgetExceeded):
        chat(openai_client(), stream=True)


async def test_async_client_raises_mapped_error(proxy):
    proxy.error = (403, "tokenrein_run_killed", "killed")
    client = guard.anthropic(anthropic.AsyncAnthropic(api_key="x"))
    async with guard.run("dead"):
        with pytest.raises(RunKilled) as ei:
            await client.messages.create(model="claude-haiku-4-5", max_tokens=8, messages=MSG)
    assert ei.value.run_id == "dead"


# --- local fallback cap ----------------------------------------------------------------------


def test_max_calls_blocks_before_sending(proxy):
    client = openai_client()
    with guard.run("capped", max_calls=2) as run:
        chat(client)
        chat(client)
        with pytest.raises(LocalCapExceeded) as ei:
            chat(client)
    assert len(proxy.requests) == 2
    assert run.calls == 2
    assert ei.value.run_id == "capped"
    assert isinstance(ei.value, BudgetExceeded)


def test_child_calls_count_against_parent_cap(proxy):
    client = openai_client()
    with guard.run("parent", max_calls=3) as parent:
        chat(client)
        with guard.run("child") as child:
            chat(client)
            chat(client)
            with pytest.raises(LocalCapExceeded) as ei:
                chat(client)
    assert ei.value.run_id == "parent"
    assert (parent.calls, child.calls) == (3, 2)
    assert len(proxy.requests) == 3


@pytest.mark.skipif(
    "retries_taken" not in inspect.signature(openai.OpenAI._build_request).parameters,
    reason="this openai version does not tell _build_request about retries",
)
def test_sdk_retries_do_not_count_against_cap(proxy):
    proxy.error = (500, "server_error", "boom")
    client = guard.openai(openai.OpenAI(api_key="x", max_retries=2))
    with guard.run("retry", max_calls=1) as run:
        with pytest.raises(openai.InternalServerError):
            chat(client)
    assert len(proxy.requests) == 3
    assert run.calls == 1


def test_user_header_from_client_or_run_and_inherited_by_child_runs(proxy):
    named = openai_client(user="alice")
    unnamed = openai_client()
    with guard.run("u1", user="bob"):
        chat(named)
        chat(unnamed)
        with guard.run("u2"):
            chat(unnamed)
    chat(unnamed)
    assert [r["headers"].get("x-tr-user") for r in proxy.requests] == ["alice", "bob", "bob", None]
    with pytest.raises(ValueError):
        guard.run(user="not valid")

from __future__ import annotations

import asyncio

from warden import HookEvent, HookEventKind
from warden.client import bind_client
from warden.modules import claude, codex


class FakeWarden:
    def __init__(self):
        self.requests = []

    async def request(self, method, params=None):
        self.requests.append((method, params))
        return {"method": method, "provider": params["provider"]}


def event():
    return HookEvent(
        sequence=5,
        kind=HookEventKind.AGENT_MESSAGE_COMPLETED,
        thread_id="thread-5",
        turn_id="turn-5",
        item_id="item-5",
        payload={"text": "done"},
        raw_method="item/completed",
        raw_payload={"item": {"type": "agentMessage"}},
        unix_receipt_ms=500,
        emitted_at_ms=490,
    )


def test_fresh_agent_helpers_request_host_service_with_full_event():
    async def scenario():
        host = FakeWarden()
        result = await claude.run(event(), prompt="find risks", warden=host)
        return host, result

    host, result = asyncio.run(scenario())

    assert result == {"method": "agent.run", "provider": "claude"}
    method, params = host.requests[0]
    assert method == "agent.run"
    assert params["provider"] == "claude"
    assert params["prompt"] == "find risks"
    assert params["event"] == event().to_dict()
    assert "options" not in params
    assert "model" not in params


def test_claude_helpers_send_trimmed_model_for_fresh_and_persistent_calls():
    async def scenario():
        host = FakeWarden()
        await claude.run(event(), model="  sonnet  ", warden=host)
        session = claude.session("reviewer", model="sonnet", warden=host)
        await session.send(event())
        return host

    host = asyncio.run(scenario())

    assert host.requests[0][1]["model"] == "sonnet"
    assert host.requests[1][1]["model"] == "sonnet"


def test_claude_helpers_reject_empty_or_non_string_models():
    async def empty_fresh():
        await claude.run(event(), model="  ", warden=FakeWarden())

    try:
        asyncio.run(empty_fresh())
    except ValueError as error:
        assert "non-empty" in str(error)
    else:
        raise AssertionError("empty model must fail")

    try:
        claude.session("reviewer", model=42)
    except TypeError as error:
        assert "string or None" in str(error)
    else:
        raise AssertionError("non-string model must fail")


def test_named_session_requests_persistent_host_service_and_lifecycle_operations():
    async def scenario():
        host = FakeWarden()
        session = codex.session("reviewer", prompt="review continuously", warden=host)
        await session.send(event())
        await session.status()
        await session.reset()
        return host

    host = asyncio.run(scenario())

    assert [request[0] for request in host.requests] == [
        "agent.session.send",
        "agent.session.status",
        "agent.session.reset",
    ]
    send = host.requests[0][1]
    assert send["provider"] == "codex"
    assert send["name"] == "reviewer"
    assert send["prompt"] == "review continuously"
    assert send["event"]["sequence"] == 5


def test_agent_helper_uses_worker_bound_client_when_not_passed_explicitly():
    async def scenario():
        host = FakeWarden()
        with bind_client(host):
            result = await claude.run(event())
        return host, result

    host, result = asyncio.run(scenario())

    assert result["provider"] == "claude"
    assert host.requests[0][0] == "agent.run"

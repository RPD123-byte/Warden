from __future__ import annotations

import asyncio
import json
from pathlib import Path
import socket
import threading
import uuid

import pytest

from warden import (
    WardenAction,
    WardenClient,
    WardenConfigurationError,
    WardenProtocolError,
    WardenRemoteError,
)


def test_async_request_uses_authenticated_jsonl_envelope():
    async def scenario():
        socket_path = _socket_path()
        received = []

        async def handle(reader, writer):
            request = json.loads(await reader.readline())
            received.append(request)
            writer.write(
                (
                    json.dumps(
                        {
                            "type": "response",
                            "protocol_version": 1,
                            "id": request["id"],
                            "ok": True,
                            "result": {"accepted": True},
                        }
                    )
                    + "\n"
                ).encode()
            )
            await writer.drain()
            writer.close()
            await writer.wait_closed()

        server = await asyncio.start_unix_server(handle, path=socket_path)
        try:
            client = WardenClient(
                str(socket_path), invocation_id="inv-1", token="secret"
            )
            result = await client.action(WardenAction.TURN_INTERRUPT)
        finally:
            server.close()
            await server.wait_closed()
            socket_path.unlink(missing_ok=True)

        assert result == {"accepted": True}
        request = received[0]
        assert request["type"] == "request"
        assert request["protocol_version"] == 1
        assert request["method"] == "warden.action"
        assert request["params"] == {
            "name": "turn_interrupt",
            "arguments": {},
        }
        assert request["context"] == {
            "invocation_id": "inv-1",
            "token": "secret",
        }

    asyncio.run(scenario())


def test_sync_request_uses_same_protocol():
    socket_path = _socket_path()
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(socket_path))
    listener.listen(1)
    received = []

    def host():
        connection, _ = listener.accept()
        with connection:
            line = b""
            while not line.endswith(b"\n"):
                line += connection.recv(4096)
            request = json.loads(line)
            received.append(request)
            connection.sendall(
                (
                    json.dumps(
                        {
                            "type": "response",
                            "protocol_version": 1,
                            "id": request["id"],
                            "ok": True,
                            "result": "done",
                        }
                    )
                    + "\n"
                ).encode()
            )
        listener.close()

    thread = threading.Thread(target=host)
    thread.start()
    try:
        client = WardenClient(str(socket_path))
        assert client.action_sync("current_event") == "done"
    finally:
        thread.join(timeout=3)
        socket_path.unlink(missing_ok=True)

    assert received[0]["params"]["name"] == "current_event"


def test_remote_failure_retains_code_message_and_details():
    async def scenario():
        socket_path = _socket_path()

        async def handle(reader, writer):
            request = json.loads(await reader.readline())
            writer.write(
                (
                    json.dumps(
                        {
                            "type": "response",
                            "protocol_version": 1,
                            "id": request["id"],
                            "ok": False,
                            "error": {
                                "code": "capability_denied",
                                "message": "not granted",
                                "details": {"action": "thread_list"},
                            },
                        }
                    )
                    + "\n"
                ).encode()
            )
            await writer.drain()
            writer.close()
            await writer.wait_closed()

        server = await asyncio.start_unix_server(handle, path=socket_path)
        try:
            with pytest.raises(WardenRemoteError) as caught:
                await WardenClient(str(socket_path)).list_threads()
        finally:
            server.close()
            await server.wait_closed()
            socket_path.unlink(missing_ok=True)

        assert caught.value.code == "capability_denied"
        assert caught.value.details == {"action": "thread_list"}

    asyncio.run(scenario())


def test_client_without_socket_fails_only_when_a_host_service_is_requested():
    client = WardenClient(None)
    derived = client.for_invocation("inv-2", "token-2")

    assert derived.invocation_id == "inv-2"
    with pytest.raises(WardenConfigurationError, match="WARDEN_SOCKET"):
        asyncio.run(derived.current_event())


def test_client_validates_history_ranges_and_action_names_before_io():
    client = WardenClient(None)

    with pytest.raises(ValueError, match="non-negative"):
        asyncio.run(client.current_thread_history(after=-1))
    with pytest.raises(ValueError, match="unsupported Warden action"):
        asyncio.run(client.action("not_a_warden_action"))


@pytest.mark.parametrize(
    ("changes", "message"),
    [
        ({"type": "notification"}, "response type"),
        ({"type": None}, "response type"),
        ({"protocol_version": 2}, "protocol version"),
        ({"protocol_version": True}, "protocol version"),
        ({"protocol_version": None}, "protocol version"),
    ],
)
def test_client_rejects_wrong_response_type_or_protocol(changes, message):
    response = {
        "type": "response",
        "protocol_version": 1,
        "id": "request-1",
        "ok": True,
        "result": "done",
    }
    response.update(changes)
    line = (json.dumps(response) + "\n").encode()

    with pytest.raises(WardenProtocolError, match=message):
        WardenClient(None)._decode_response("request-1", line)


def _socket_path() -> Path:
    # Darwin limits AF_UNIX paths to 104 bytes; pytest's tmp_path can exceed it.
    return Path("/tmp") / f"warden-{uuid.uuid4().hex[:12]}.sock"

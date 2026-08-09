"""Authenticated JSONL client for services owned by the Warden host daemon."""

from __future__ import annotations

import asyncio
from contextlib import contextmanager
from contextvars import ContextVar
from dataclasses import dataclass, replace
import json
import os
import socket
from typing import Any, Iterator, Mapping
import uuid

from .actions import WardenAction


PROTOCOL_VERSION = 1
DEFAULT_MAX_MESSAGE_BYTES = 4 * 1024 * 1024
DEFAULT_TIMEOUT_SECONDS = 30.0


class WardenError(RuntimeError):
    """Base error for Warden host communication."""


class WardenConfigurationError(WardenError):
    """The hook worker was not configured with a usable Warden socket."""


class WardenProtocolError(WardenError):
    """The host returned malformed or mismatched JSONL."""


class WardenRemoteError(WardenError):
    """The Warden host rejected or failed a request."""

    def __init__(self, code: str, message: str, details: Any = None) -> None:
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message
        self.details = details


@dataclass(frozen=True, slots=True)
class WardenClient:
    """A thin client; the Rust daemon remains owner of all actions and agents."""

    socket_path: str | None
    invocation_id: str | None = None
    token: str | None = None
    timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS
    max_message_bytes: int = DEFAULT_MAX_MESSAGE_BYTES

    @classmethod
    def from_env(cls) -> WardenClient:
        timeout = _positive_float_env(
            "WARDEN_REQUEST_TIMEOUT_SECONDS", DEFAULT_TIMEOUT_SECONDS
        )
        max_bytes = _positive_int_env(
            "WARDEN_MAX_MESSAGE_BYTES", DEFAULT_MAX_MESSAGE_BYTES
        )
        return cls(
            socket_path=os.environ.get("WARDEN_SOCKET"),
            invocation_id=os.environ.get("WARDEN_INVOCATION_ID"),
            token=os.environ.get("WARDEN_INVOCATION_TOKEN"),
            timeout_seconds=timeout,
            max_message_bytes=max_bytes,
        )

    def for_invocation(
        self, invocation_id: str | None, token: str | None
    ) -> WardenClient:
        return replace(
            self,
            invocation_id=invocation_id
            if invocation_id is not None
            else self.invocation_id,
            token=token if token is not None else self.token,
        )

    async def request(
        self, method: str, params: Mapping[str, Any] | None = None
    ) -> Any:
        request = self._request_message(method, params)
        encoded = self._encode(request)
        socket_path = self._socket_path()

        try:
            reader, writer = await asyncio.wait_for(
                asyncio.open_unix_connection(
                    socket_path, limit=self.max_message_bytes + 1
                ),
                timeout=self.timeout_seconds,
            )
        except (OSError, TimeoutError) as error:
            raise WardenError(
                f"could not connect to Warden socket {socket_path!r}: {error}"
            ) from error

        try:
            writer.write(encoded)
            await asyncio.wait_for(writer.drain(), timeout=self.timeout_seconds)
            line = await asyncio.wait_for(
                reader.readline(), timeout=self.timeout_seconds
            )
        except (OSError, TimeoutError, ValueError) as error:
            raise WardenError(f"Warden request failed: {error}") from error
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except OSError:
                pass

        return self._decode_response(request["id"], line)

    def request_sync(self, method: str, params: Mapping[str, Any] | None = None) -> Any:
        request = self._request_message(method, params)
        encoded = self._encode(request)
        socket_path = self._socket_path()

        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
                connection.settimeout(self.timeout_seconds)
                connection.connect(socket_path)
                connection.sendall(encoded)
                line = _receive_line(connection, self.max_message_bytes)
        except OSError as error:
            raise WardenError(f"Warden request failed: {error}") from error

        return self._decode_response(request["id"], line)

    async def action(self, name: WardenAction | str, **arguments: Any) -> Any:
        action = WardenAction.parse(name)
        return await self.request(
            "warden.action", {"name": action.value, "arguments": arguments}
        )

    def action_sync(self, name: WardenAction | str, **arguments: Any) -> Any:
        action = WardenAction.parse(name)
        return self.request_sync(
            "warden.action", {"name": action.value, "arguments": arguments}
        )

    async def current_event(self) -> Any:
        return await self.action("current_event")

    async def current_thread_snapshot(self) -> Any:
        return await self.action("current_thread_snapshot")

    async def current_thread_history(
        self, *, after: int | None = None, through: int | None = None
    ) -> Any:
        arguments = _history_arguments(after, through)
        return await self.action("current_thread_history", **arguments)

    async def start_turn(self, input: Any) -> Any:
        return await self.action("turn_start", input=input)

    async def steer_turn(self, input: Any) -> Any:
        return await self.action("turn_steer", input=input)

    async def interrupt_turn(self) -> Any:
        return await self.action("turn_interrupt")

    async def list_threads(self) -> Any:
        return await self.action("thread_list")

    async def arbitrary_thread_snapshot(self, thread_id: str) -> Any:
        return await self.action("arbitrary_thread_snapshot", thread_id=thread_id)

    async def arbitrary_thread_history(
        self,
        thread_id: str,
        *,
        after: int | None = None,
        through: int | None = None,
    ) -> Any:
        return await self.action(
            "arbitrary_thread_history",
            thread_id=thread_id,
            **_history_arguments(after, through),
        )

    async def arbitrary_turn_start(self, thread_id: str, input: Any) -> Any:
        return await self.action(
            "arbitrary_turn_start", thread_id=thread_id, input=input
        )

    async def arbitrary_turn_steer(
        self, thread_id: str, turn_id: str, input: Any
    ) -> Any:
        return await self.action(
            "arbitrary_turn_steer",
            thread_id=thread_id,
            turn_id=turn_id,
            input=input,
        )

    async def arbitrary_turn_interrupt(self, thread_id: str, turn_id: str) -> Any:
        return await self.action(
            "arbitrary_turn_interrupt", thread_id=thread_id, turn_id=turn_id
        )

    def _socket_path(self) -> str:
        if not self.socket_path:
            raise WardenConfigurationError(
                "WARDEN_SOCKET is not set for this hook invocation"
            )
        return self.socket_path

    def _request_message(
        self, method: str, params: Mapping[str, Any] | None
    ) -> dict[str, Any]:
        if not isinstance(method, str) or not method:
            raise ValueError("method must be a non-empty string")
        if params is not None and not isinstance(params, Mapping):
            raise TypeError("params must be a mapping")
        return {
            "type": "request",
            "protocol_version": PROTOCOL_VERSION,
            "id": uuid.uuid4().hex,
            "method": method,
            "params": dict(params or {}),
            "context": {
                "invocation_id": self.invocation_id,
                "token": self.token,
            },
        }

    def _encode(self, message: Mapping[str, Any]) -> bytes:
        try:
            encoded = (json.dumps(message, separators=(",", ":")) + "\n").encode(
                "utf-8"
            )
        except (TypeError, ValueError) as error:
            raise WardenProtocolError(
                f"request is not JSON serializable: {error}"
            ) from error
        if len(encoded) > self.max_message_bytes:
            raise WardenProtocolError("request exceeds WARDEN_MAX_MESSAGE_BYTES")
        return encoded

    def _decode_response(self, request_id: str, line: bytes) -> Any:
        if not line:
            raise WardenProtocolError("Warden closed the socket without a response")
        if len(line) > self.max_message_bytes:
            raise WardenProtocolError(
                "Warden response exceeds WARDEN_MAX_MESSAGE_BYTES"
            )
        if not line.endswith(b"\n"):
            raise WardenProtocolError("Warden response is not newline terminated")
        try:
            response = json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise WardenProtocolError(
                f"Warden response is invalid JSON: {error}"
            ) from error
        if not isinstance(response, dict):
            raise WardenProtocolError("Warden response must be a JSON object")
        if response.get("type") != "response":
            raise WardenProtocolError("Warden response type must be response")
        version = response.get("protocol_version")
        if (
            isinstance(version, bool)
            or not isinstance(version, int)
            or version != PROTOCOL_VERSION
        ):
            raise WardenProtocolError(
                f"Warden response protocol version must be {PROTOCOL_VERSION}"
            )
        if response.get("id") != request_id:
            raise WardenProtocolError("Warden response id does not match request id")
        if response.get("ok") is True:
            return response.get("result")
        if response.get("ok") is not False:
            raise WardenProtocolError("Warden response must contain a boolean ok field")

        error = response.get("error")
        if not isinstance(error, dict):
            raise WardenProtocolError(
                "failed Warden response must contain an error object"
            )
        code = error.get("code", "host_error")
        message = error.get("message", "Warden request failed")
        if not isinstance(code, str) or not isinstance(message, str):
            raise WardenProtocolError("Warden error code and message must be strings")
        raise WardenRemoteError(code, message, error.get("details"))


Warden = WardenClient
_CURRENT_CLIENT: ContextVar[WardenClient | None] = ContextVar(
    "warden_current_client", default=None
)


def get_current_client() -> WardenClient:
    client = _CURRENT_CLIENT.get()
    if client is None:
        raise WardenConfigurationError(
            "no Warden client is bound; call from a running hook or pass warden= explicitly"
        )
    return client


@contextmanager
def bind_client(client: WardenClient) -> Iterator[None]:
    """Bind a client for reusable modules during one hook invocation."""

    token = _CURRENT_CLIENT.set(client)
    try:
        yield
    finally:
        _CURRENT_CLIENT.reset(token)


def _receive_line(connection: socket.socket, max_bytes: int) -> bytes:
    chunks = bytearray()
    while len(chunks) <= max_bytes:
        chunk = connection.recv(min(65536, max_bytes + 1 - len(chunks)))
        if not chunk:
            break
        chunks.extend(chunk)
        newline = chunks.find(b"\n")
        if newline >= 0:
            return bytes(chunks[: newline + 1])
    if len(chunks) > max_bytes:
        raise WardenProtocolError("Warden response exceeds WARDEN_MAX_MESSAGE_BYTES")
    return bytes(chunks)


def _positive_float_env(name: str, default: float) -> float:
    raw = os.environ.get(name)
    if raw is None:
        return default
    try:
        value = float(raw)
    except ValueError as error:
        raise WardenConfigurationError(f"{name} must be a number") from error
    if value <= 0:
        raise WardenConfigurationError(f"{name} must be positive")
    return value


def _positive_int_env(name: str, default: int) -> int:
    raw = os.environ.get(name)
    if raw is None:
        return default
    try:
        value = int(raw)
    except ValueError as error:
        raise WardenConfigurationError(f"{name} must be an integer") from error
    if value <= 0:
        raise WardenConfigurationError(f"{name} must be positive")
    return value


def _history_arguments(after: int | None, through: int | None) -> dict[str, int]:
    arguments: dict[str, int] = {}
    for name, value in (("after", after), ("through", through)):
        if value is None:
            continue
        if isinstance(value, bool) or not isinstance(value, int) or value < 0:
            raise ValueError(f"{name} must be a non-negative integer")
        arguments[name] = value
    return arguments

"""Supervised JSONL worker for one immutable Warden hook revision."""

from __future__ import annotations

import argparse
import asyncio
from contextlib import redirect_stderr, redirect_stdout
from dataclasses import dataclass
import importlib.util
import inspect
import io
import json
import os
from pathlib import Path
import sys
import traceback
from typing import Any, Mapping, Sequence, TextIO
import uuid

from .client import PROTOCOL_VERSION, WardenClient, bind_client
from .events import HookEvent
from .hooks import HookFunction, HookMetadata, find_hook, hook_arity
from .modules._agent import (
    _persistent_session_declarations,
    _reset_persistent_session_declarations,
)


DEFAULT_MAX_MESSAGE_BYTES = 4 * 1024 * 1024
DEFAULT_MAX_LOG_CHARS = 64 * 1024
_PROTOCOL_STDOUT: TextIO = sys.stdout


class BoundedTextCapture:
    """Text stream that retains only a bounded prefix while counting discarded text."""

    encoding = "utf-8"
    errors = None

    def __init__(self, limit: int) -> None:
        if isinstance(limit, bool) or not isinstance(limit, int) or limit <= 0:
            raise ValueError("capture limit must be a positive integer")
        self._limit = limit
        self._buffer = io.StringIO()
        self._omitted_characters = 0

    @property
    def stored_characters(self) -> int:
        return self._buffer.tell()

    @property
    def omitted_characters(self) -> int:
        return self._omitted_characters

    def write(self, value: str) -> int:
        if not isinstance(value, str):
            raise TypeError("captured output must be text")
        remaining = self._limit - self.stored_characters
        retained = min(len(value), max(remaining, 0))
        if retained:
            self._buffer.write(value[:retained])
        self._omitted_characters += len(value) - retained
        return len(value)

    def flush(self) -> None:
        pass

    def isatty(self) -> bool:
        return False

    def render(self) -> str:
        value = self._buffer.getvalue()
        if not self._omitted_characters:
            return value
        return value + f"\n...[truncated {self._omitted_characters} characters]"


@dataclass(frozen=True, slots=True)
class LoadedHook:
    name: str
    function: HookFunction
    metadata: HookMetadata
    import_stdout: str
    import_stderr: str
    persistent_agent_sessions: bool


class WorkerInputError(ValueError):
    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code


def load_hook(
    path: Path,
    name: str | None = None,
    *,
    max_log_chars: int = DEFAULT_MAX_LOG_CHARS,
) -> LoadedHook:
    resolved = path.expanduser().resolve()
    if not resolved.is_file():
        raise ValueError(f"hook module does not exist: {resolved}")
    if not name:
        name = resolved.parent.name
    if not name:
        raise ValueError("hook name cannot be empty")

    module_name = f"_warden_hook_{uuid.uuid4().hex}"
    spec = importlib.util.spec_from_file_location(module_name, resolved)
    if spec is None or spec.loader is None:
        raise ValueError(f"could not load hook module: {resolved}")
    module = importlib.util.module_from_spec(spec)

    stdout = BoundedTextCapture(max_log_chars)
    stderr = BoundedTextCapture(max_log_chars)
    sys.modules[module_name] = module
    search_paths = [str(resolved.parent)]
    modules_root = os.environ.get("WARDEN_MODULES_ROOT")
    if modules_root and Path(modules_root).is_dir():
        search_paths.insert(0, str(Path(modules_root).resolve()))
    for search_path in reversed(search_paths):
        sys.path.insert(0, search_path)
    _reset_persistent_session_declarations()
    try:
        with redirect_stdout(stdout), redirect_stderr(stderr):
            spec.loader.exec_module(module)
    except BaseException:
        sys.modules.pop(module_name, None)
        raise
    finally:
        for search_path in search_paths:
            try:
                sys.path.remove(search_path)
            except ValueError:
                pass

    function, metadata = find_hook(vars(module))
    persistent_agent_sessions = bool(_persistent_session_declarations())
    return LoadedHook(
        name=name,
        function=function,
        metadata=metadata,
        import_stdout=stdout.render(),
        import_stderr=stderr.render(),
        persistent_agent_sessions=persistent_agent_sessions,
    )


async def serve(hook_path: Path, hook_name: str | None = None) -> int:
    max_message_bytes = _positive_int_env(
        "WARDEN_MAX_MESSAGE_BYTES", DEFAULT_MAX_MESSAGE_BYTES
    )
    max_log_chars = _positive_int_env("WARDEN_MAX_LOG_CHARS", DEFAULT_MAX_LOG_CHARS)

    try:
        base_client = WardenClient.from_env()
        loaded = load_hook(hook_path, hook_name, max_log_chars=max_log_chars)
    except BaseException as error:
        _emit(
            {
                "type": "handshake",
                "protocol_version": PROTOCOL_VERSION,
                "ok": False,
                "error": _error("hook_load_failed", error),
            },
            max_message_bytes,
        )
        return 1

    _emit(
        {
            "type": "handshake",
            "protocol_version": PROTOCOL_VERSION,
            "ok": True,
            "hook": {
                "name": loaded.name,
                "function": loaded.function.__name__,
                "events": [event.value for event in loaded.metadata.events],
                "actions": [action.value for action in loaded.metadata.actions],
                "blocking": loaded.metadata.blocking,
                "persistent_agent_sessions": loaded.persistent_agent_sessions,
                "is_async": inspect.iscoroutinefunction(loaded.function),
            },
            "logs": {
                "stdout": loaded.import_stdout,
                "stderr": loaded.import_stderr,
            },
        },
        max_message_bytes,
    )

    while True:
        line = await asyncio.to_thread(sys.stdin.buffer.readline, max_message_bytes + 1)
        if not line:
            return 0
        if len(line) > max_message_bytes or not line.endswith(b"\n"):
            _emit(
                {
                    "type": "error",
                    "ok": False,
                    "error": {
                        "code": "message_too_large",
                        "message": "worker input exceeds WARDEN_MAX_MESSAGE_BYTES",
                    },
                },
                max_message_bytes,
            )
            return 2

        try:
            message = json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            _emit(
                {
                    "type": "error",
                    "ok": False,
                    "error": _error("invalid_json", error, include_traceback=False),
                },
                max_message_bytes,
            )
            continue

        if not isinstance(message, dict):
            _emit(
                {
                    "type": "error",
                    "ok": False,
                    "error": {
                        "code": "invalid_message",
                        "message": "worker input must be a JSON object",
                    },
                },
                max_message_bytes,
            )
            continue

        if message.get("type") == "shutdown":
            _emit(
                {"type": "shutdown", "id": message.get("id"), "ok": True},
                max_message_bytes,
            )
            return 0

        response = await _invoke(loaded, base_client, message, max_log_chars)
        _emit(response, max_message_bytes)


async def _invoke(
    loaded: LoadedHook,
    base_client: WardenClient,
    message: Mapping[str, Any],
    max_log_chars: int,
) -> dict[str, Any]:
    request_id = message.get("id")
    try:
        if message.get("type") != "invoke":
            raise WorkerInputError(
                "invalid_message", "worker message type must be invoke"
            )
        if not isinstance(request_id, str) or not request_id:
            raise WorkerInputError(
                "invalid_message", "invoke.id must be a non-empty string"
            )
        version = message.get("protocol_version", PROTOCOL_VERSION)
        if version != PROTOCOL_VERSION:
            raise WorkerInputError(
                "unsupported_protocol",
                f"worker protocol version {version!r} is not supported",
            )
        event_value = message.get("event")
        if not isinstance(event_value, Mapping):
            raise WorkerInputError(
                "invalid_event", "invoke.event must be a JSON object"
            )
        try:
            event = HookEvent.from_dict(event_value)
        except (TypeError, ValueError) as error:
            raise WorkerInputError("invalid_event", str(error)) from error
        if event.kind not in loaded.metadata.events:
            raise WorkerInputError(
                "event_not_registered",
                f"hook {loaded.name!r} does not handle {event.kind.value!r}",
            )

        context = message.get("warden", {})
        if context is None:
            context = {}
        if not isinstance(context, Mapping):
            raise WorkerInputError("invalid_message", "invoke.warden must be an object")
        invocation_id = _optional_string(context.get("invocation_id"), "invocation_id")
        token = _optional_string(context.get("token"), "token")
        client = base_client.for_invocation(invocation_id, token)
    except WorkerInputError as error:
        return {
            "type": "result",
            "id": request_id,
            "ok": False,
            "error": {"code": error.code, "message": str(error)},
        }

    stdout = BoundedTextCapture(max_log_chars)
    stderr = BoundedTextCapture(max_log_chars)
    try:
        with bind_client(client), redirect_stdout(stdout), redirect_stderr(stderr):
            if hook_arity(loaded.function) == 1:
                result = loaded.function(event)
            else:
                result = loaded.function(event, client)
            if inspect.isawaitable(result):
                result = await result
    except asyncio.CancelledError:
        raise
    except BaseException as error:
        return {
            "type": "result",
            "id": request_id,
            "ok": False,
            "logs": _logs(stdout, stderr),
            "error": _error("hook_exception", error),
        }

    try:
        json.dumps(result)
    except (TypeError, ValueError) as error:
        return {
            "type": "result",
            "id": request_id,
            "ok": False,
            "logs": _logs(stdout, stderr),
            "error": _error("invalid_hook_result", error, include_traceback=False),
        }

    return {
        "type": "result",
        "id": request_id,
        "ok": True,
        "result": result,
        "logs": _logs(stdout, stderr),
    }


def _emit(message: dict[str, Any], max_message_bytes: int) -> None:
    encoded = _encode(message)
    if len(encoded) > max_message_bytes:
        encoded = _encode(
            {
                "type": "result" if message.get("type") == "result" else "error",
                "id": message.get("id"),
                "ok": False,
                "error": {
                    "code": "response_too_large",
                    "message": "worker response exceeds WARDEN_MAX_MESSAGE_BYTES",
                },
            }
        )
    _PROTOCOL_STDOUT.buffer.write(encoded)
    _PROTOCOL_STDOUT.flush()


def _encode(message: Mapping[str, Any]) -> bytes:
    return (json.dumps(message, separators=(",", ":")) + "\n").encode("utf-8")


def _logs(stdout: BoundedTextCapture, stderr: BoundedTextCapture) -> dict[str, str]:
    return {
        "stdout": stdout.render(),
        "stderr": stderr.render(),
    }


def _error(
    code: str, error: BaseException, *, include_traceback: bool = True
) -> dict[str, Any]:
    value: dict[str, Any] = {
        "code": code,
        "message": str(error) or type(error).__name__,
        "exception_type": type(error).__name__,
    }
    if include_traceback:
        value["traceback"] = "".join(
            traceback.format_exception(type(error), error, error.__traceback__)
        )
    return value


def _optional_string(value: Any, field: str) -> str | None:
    if value is None:
        return None
    if not isinstance(value, str) or not value:
        raise WorkerInputError(
            "invalid_message", f"invoke.warden.{field} must be a string"
        )
    return value


def _positive_int_env(name: str, default: int) -> int:
    raw = os.environ.get(name)
    if raw is None:
        return default
    try:
        value = int(raw)
    except ValueError as error:
        raise ValueError(f"{name} must be an integer") from error
    if value <= 0:
        raise ValueError(f"{name} must be positive")
    return value


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hook", required=True, type=Path, help="path to hook.py")
    parser.add_argument("--hook-name", help="validated Warden hook identity")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    return asyncio.run(serve(args.hook, args.hook_name))


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Fail-open Codex native-hook bridge for the local Warden daemon."""

from __future__ import annotations

import argparse
import json
import socket
import sys
import uuid
from pathlib import Path


MAX_MESSAGE_BYTES = 1024 * 1024


def _diagnostic(message: str) -> None:
    print(f"warden bridge: {message}"[:2048], file=sys.stderr, flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--socket", required=True)
    parser.add_argument("--credential-file", required=True)
    parser.add_argument("--timeout", type=float, default=605.0)
    try:
        arguments = parser.parse_args()
        raw = sys.stdin.buffer.read(MAX_MESSAGE_BYTES + 1)
        if not raw or len(raw) > MAX_MESSAGE_BYTES:
            raise ValueError("native payload is empty or too large")
        payload = json.loads(raw)
        if not isinstance(payload, dict):
            raise ValueError("native payload must be a JSON object")
        for field in ("hook_event_name", "session_id", "turn_id"):
            if not isinstance(payload.get(field), str) or not payload[field]:
                raise ValueError(f"native payload has no valid {field}")

        credential = Path(arguments.credential_file).read_text(encoding="utf-8").strip()
        if not credential:
            raise ValueError("bridge credential is empty")
        request_id = uuid.uuid4().hex
        request = {
            "type": "request",
            "protocol_version": 1,
            "id": request_id,
            "method": "warden.native_hook.event",
            "params": payload,
            "context": None,
            "bridge_auth": credential,
        }
        encoded = json.dumps(request, separators=(",", ":")).encode("utf-8") + b"\n"
        if len(encoded) > MAX_MESSAGE_BYTES:
            raise ValueError("bridge request is too large")

        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(arguments.timeout)
            stream.connect(arguments.socket)
            stream.sendall(encoded)
            response = bytearray()
            while len(response) <= MAX_MESSAGE_BYTES:
                chunk = stream.recv(min(65536, MAX_MESSAGE_BYTES + 1 - len(response)))
                if not chunk:
                    break
                response.extend(chunk)
                if b"\n" in chunk:
                    break
        if not response.endswith(b"\n") or len(response) > MAX_MESSAGE_BYTES:
            raise ValueError("daemon response is empty, unterminated, or too large")
        decoded = json.loads(response)
        if decoded.get("id") != request_id or decoded.get("ok") is not True:
            error = decoded.get("error") or "daemon rejected bridge event"
            raise ValueError(str(error))
    except Exception as error:  # Native hooks must never strand Codex.
        _diagnostic(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

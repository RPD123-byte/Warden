"""Stable event values delivered to Python hooks."""

from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum
import re
from typing import Any, Mapping


class HookEventKind(StrEnum):
    """Normalized lifecycle-complete Codex event kinds supported by Warden."""

    USER_PROMPT_SUBMITTED = "user_prompt_submitted"
    TURN_STARTED = "turn_started"
    PRE_TOOL_USE = "pre_tool_use"
    POST_TOOL_USE = "post_tool_use"
    POST_TOOL_USE_FAILURE = "post_tool_use_failure"
    AGENT_MESSAGE_COMPLETED = "agent_message_completed"
    TURN_COMPLETED = "turn_completed"
    TURN_FAILED = "turn_failed"
    TURN_INTERRUPTED = "turn_interrupted"
    UNKNOWN_UPSTREAM_EVENT = "unknown_upstream_event"

    @classmethod
    def parse(cls, value: HookEventKind | str) -> HookEventKind:
        if isinstance(value, cls):
            return value
        if not isinstance(value, str) or not value.strip():
            raise ValueError("hook event kind must be a non-empty string")

        candidate = value.strip()
        try:
            return cls(candidate)
        except ValueError:
            pass

        try:
            return cls[candidate.upper()]
        except KeyError:
            pass

        enum_name = re.sub(r"(?<!^)(?=[A-Z])", "_", candidate).upper()
        try:
            return cls[enum_name]
        except KeyError as error:
            raise ValueError(f"unsupported hook event kind: {value!r}") from error

    @classmethod
    def parse_incoming(cls, value: Any) -> HookEventKind:
        """Parse a host value while preserving forward compatibility as unknown."""

        try:
            return cls.parse(value)
        except ValueError:
            return cls.UNKNOWN_UPSTREAM_EVENT


@dataclass(frozen=True, slots=True)
class HookEvent:
    """Faithful subprocess representation of one normalized source event."""

    sequence: int
    kind: HookEventKind
    thread_id: str | None
    turn_id: str | None
    item_id: str | None
    payload: Any
    raw_method: str | None
    raw_payload: Any
    unix_receipt_ms: int | None = None
    emitted_at_ms: int | None = None
    reconstructed: bool = False
    origin: str = "observed"
    source_sequence: int | None = None
    receipt_ordinal: int | None = None
    native_event_name: str | None = None

    def __post_init__(self) -> None:
        if self.source_sequence is None and self.origin == "observed":
            object.__setattr__(self, "source_sequence", self.sequence)
        if self.receipt_ordinal is None:
            object.__setattr__(self, "receipt_ordinal", self.sequence)

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> HookEvent:
        if not isinstance(value, Mapping):
            raise TypeError("event must be a JSON object")

        sequence = value.get("sequence")
        if isinstance(sequence, bool) or not isinstance(sequence, int) or sequence < 0:
            raise ValueError("event.sequence must be a non-negative integer")

        thread_id = _optional_string(value.get("thread_id"), "event.thread_id")
        turn_id = _optional_string(value.get("turn_id"), "event.turn_id")
        item_id = _optional_string(value.get("item_id"), "event.item_id")
        raw_method = _optional_string(value.get("raw_method"), "event.raw_method")
        unix_receipt_ms = _optional_non_negative_int(
            value.get("unix_receipt_ms"), "event.unix_receipt_ms"
        )
        emitted_at_ms = _optional_non_negative_int(
            value.get("emitted_at_ms"), "event.emitted_at_ms"
        )
        reconstructed = value.get("reconstructed", False)
        if not isinstance(reconstructed, bool):
            raise ValueError("event.reconstructed must be a boolean")

        origin = value.get("origin", "observed")
        if origin not in {"observed", "native"}:
            raise ValueError("event.origin must be 'observed' or 'native'")
        source_sequence = _optional_non_negative_int(
            value.get("source_sequence", sequence if origin == "observed" else None),
            "event.source_sequence",
        )
        receipt_ordinal = value.get("receipt_ordinal", sequence)
        if (
            isinstance(receipt_ordinal, bool)
            or not isinstance(receipt_ordinal, int)
            or receipt_ordinal < 0
        ):
            raise ValueError("event.receipt_ordinal must be a non-negative integer")
        native_event_name = _optional_string(
            value.get("native_event_name"), "event.native_event_name"
        )

        return cls(
            sequence=sequence,
            origin=origin,
            source_sequence=source_sequence,
            receipt_ordinal=receipt_ordinal,
            native_event_name=native_event_name,
            kind=HookEventKind.parse_incoming(value.get("kind")),
            thread_id=thread_id,
            turn_id=turn_id,
            item_id=item_id,
            payload=value.get("payload"),
            raw_method=raw_method,
            raw_payload=value.get("raw_payload"),
            unix_receipt_ms=unix_receipt_ms,
            emitted_at_ms=emitted_at_ms,
            reconstructed=reconstructed,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "sequence": self.sequence,
            "origin": self.origin,
            "source_sequence": self.source_sequence,
            "receipt_ordinal": self.receipt_ordinal,
            "native_event_name": self.native_event_name,
            "kind": self.kind.value,
            "thread_id": self.thread_id,
            "turn_id": self.turn_id,
            "item_id": self.item_id,
            "payload": self.payload,
            "raw_method": self.raw_method,
            "raw_payload": self.raw_payload,
            "unix_receipt_ms": self.unix_receipt_ms,
            "emitted_at_ms": self.emitted_at_ms,
            "reconstructed": self.reconstructed,
        }


def _optional_string(value: Any, field: str) -> str | None:
    if value is None:
        return None
    if not isinstance(value, str) or not value:
        raise ValueError(f"{field} must be null or a non-empty string")
    return value


def _optional_non_negative_int(value: Any, field: str) -> int | None:
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError(f"{field} must be null or a non-negative integer")
    return value

"""Typed names for Warden observation and control grants."""

from __future__ import annotations

from enum import StrEnum


class WardenAction(StrEnum):
    CURRENT_EVENT = "current_event"
    CURRENT_THREAD_SNAPSHOT = "current_thread_snapshot"
    CURRENT_THREAD_HISTORY = "current_thread_history"
    TURN_START = "turn_start"
    TURN_STEER = "turn_steer"
    TURN_INTERRUPT = "turn_interrupt"
    THREAD_LIST = "thread_list"
    ARBITRARY_THREAD_SNAPSHOT = "arbitrary_thread_snapshot"
    ARBITRARY_THREAD_HISTORY = "arbitrary_thread_history"
    ARBITRARY_TURN_START = "arbitrary_turn_start"
    ARBITRARY_TURN_STEER = "arbitrary_turn_steer"
    ARBITRARY_TURN_INTERRUPT = "arbitrary_turn_interrupt"

    @classmethod
    def parse(cls, value: WardenAction | str) -> WardenAction:
        if isinstance(value, cls):
            return value
        if not isinstance(value, str) or not value.strip():
            raise ValueError("Warden action must be a non-empty string")
        candidate = value.strip()
        try:
            return cls(candidate)
        except ValueError:
            pass
        try:
            return cls[candidate.upper()]
        except KeyError as error:
            raise ValueError(f"unsupported Warden action: {value!r}") from error

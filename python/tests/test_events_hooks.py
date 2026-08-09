from __future__ import annotations

import pytest

from warden import (
    HookEvent,
    HookEventKind,
    WardenAction,
    get_hook_metadata,
    hook,
)
from warden.hooks import find_hook


def event_dict(**changes):
    value = {
        "sequence": 42,
        "kind": "post_tool_use",
        "thread_id": "thread-1",
        "turn_id": "turn-1",
        "item_id": "item-1",
        "payload": {"status": "completed"},
        "raw_method": "item/completed",
        "raw_payload": {"item": {"id": "item-1"}},
        "unix_receipt_ms": 1000,
        "emitted_at_ms": 990,
        "reconstructed": False,
    }
    value.update(changes)
    return value


def test_event_round_trip_preserves_normalized_and_raw_values():
    event = HookEvent.from_dict(event_dict())

    assert event.kind is HookEventKind.POST_TOOL_USE
    assert event.sequence == 42
    assert event.raw_payload == {"item": {"id": "item-1"}}
    expected = event_dict() | {
        "origin": "observed",
        "source_sequence": 42,
        "receipt_ordinal": 42,
        "native_event_name": None,
    }
    assert event.to_dict() == expected


@pytest.mark.parametrize(
    ("value", "expected"),
    [
        ("post_tool_use", HookEventKind.POST_TOOL_USE),
        ("PostToolUse", HookEventKind.POST_TOOL_USE),
        ("POST_TOOL_USE", HookEventKind.POST_TOOL_USE),
    ],
)
def test_event_kind_accepts_wire_python_and_pascal_spellings(value, expected):
    assert HookEventKind.parse(value) is expected


def test_future_host_kind_is_represented_as_unknown_without_losing_raw_frame():
    event = HookEvent.from_dict(
        event_dict(kind="future_codex_event", raw_method="future/event")
    )

    assert event.kind is HookEventKind.UNKNOWN_UPSTREAM_EVENT
    assert event.raw_method == "future/event"


@pytest.mark.parametrize(
    "changes",
    [
        {"sequence": -1},
        {"sequence": True},
        {"thread_id": ""},
        {"turn_id": 7},
        {"raw_method": 7},
        {"unix_receipt_ms": -1},
        {"reconstructed": "false"},
    ],
)
def test_event_rejects_invalid_envelope_fields(changes):
    with pytest.raises(ValueError):
        HookEvent.from_dict(event_dict(**changes))


def test_hook_decorator_attaches_deduplicated_metadata():
    @hook(
        on=[HookEventKind.POST_TOOL_USE, "post_tool_use", "TurnCompleted"],
        actions=[WardenAction.CURRENT_EVENT, "current_event", "turn_interrupt"],
        blocking=True,
    )
    async def handle(event, warden):
        return None

    metadata = get_hook_metadata(handle)
    assert metadata is not None
    assert metadata.events == (
        HookEventKind.POST_TOOL_USE,
        HookEventKind.TURN_COMPLETED,
    )
    assert metadata.actions == (
        WardenAction.CURRENT_EVENT,
        WardenAction.TURN_INTERRUPT,
    )
    assert metadata.blocking is True
    assert metadata.to_dict() == {
        "events": ["post_tool_use", "turn_completed"],
        "actions": ["current_event", "turn_interrupt"],
        "blocking": True,
    }
    assert find_hook({"handle": handle}) == (handle, metadata)


def test_hook_decorator_requires_an_event():
    with pytest.raises(ValueError, match="at least one"):
        hook(on=[])


def test_hook_decorator_defaults_to_non_blocking():
    @hook(on=HookEventKind.TURN_STARTED)
    def handle(event):
        return None

    metadata = get_hook_metadata(handle)
    assert metadata is not None
    assert metadata.blocking is False


def test_hook_decorator_requires_boolean_blocking_value():
    with pytest.raises(TypeError, match="must be a bool"):
        hook(on=HookEventKind.TURN_STARTED, blocking="yes")  # type: ignore[arg-type]


def test_hook_decorator_rejects_unknown_action_name():
    with pytest.raises(ValueError, match="unsupported Warden action"):
        hook(on=HookEventKind.TURN_STARTED, actions=["run_arbitrary_shell"])


def test_find_hook_accepts_minimal_event_only_function():
    @hook(on=HookEventKind.TURN_STARTED)
    def minimal(event):
        return event

    function, metadata = find_hook({"minimal": minimal})
    assert function is minimal
    assert metadata.actions == ()


def test_find_hook_requires_one_valid_function():
    with pytest.raises(ValueError, match="exactly one"):
        find_hook({})

    @hook(on=HookEventKind.TURN_STARTED)
    def invalid():
        return None

    with pytest.raises(ValueError, match="event or event and warden"):
        find_hook({"invalid": invalid})

    @hook(on=HookEventKind.TURN_STARTED)
    def too_many(event, warden, required):
        return required

    with pytest.raises(ValueError, match="event or event and warden"):
        find_hook({"too_many": too_many})

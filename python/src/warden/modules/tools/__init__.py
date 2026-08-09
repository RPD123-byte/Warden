"""Reusable, invocation-scoped facades for Warden actions.

Every function delegates to the authenticated client bound to the current hook.
Pass ``warden=`` only when calling from code that receives an explicit client.
The hook's ``actions=[...]`` declaration remains the source of authority.
"""

from __future__ import annotations

from typing import Any

from ...client import WardenClient, get_current_client


def _client(warden: WardenClient | None) -> WardenClient:
    return warden or get_current_client()


async def current_event(*, warden: WardenClient | None = None) -> Any:
    return await _client(warden).current_event()


async def current_thread_snapshot(*, warden: WardenClient | None = None) -> Any:
    return await _client(warden).current_thread_snapshot()


async def current_thread_history(
    *,
    after: int | None = None,
    through: int | None = None,
    warden: WardenClient | None = None,
) -> Any:
    return await _client(warden).current_thread_history(after=after, through=through)


async def start_turn(input: Any, *, warden: WardenClient | None = None) -> Any:
    return await _client(warden).start_turn(input)


async def steer_turn(input: Any, *, warden: WardenClient | None = None) -> Any:
    return await _client(warden).steer_turn(input)


async def interrupt_turn(*, warden: WardenClient | None = None) -> Any:
    return await _client(warden).interrupt_turn()


async def list_threads(*, warden: WardenClient | None = None) -> Any:
    return await _client(warden).list_threads()


async def arbitrary_thread_snapshot(
    thread_id: str, *, warden: WardenClient | None = None
) -> Any:
    return await _client(warden).arbitrary_thread_snapshot(thread_id)


async def arbitrary_thread_history(
    thread_id: str,
    *,
    after: int | None = None,
    through: int | None = None,
    warden: WardenClient | None = None,
) -> Any:
    return await _client(warden).arbitrary_thread_history(
        thread_id, after=after, through=through
    )


async def arbitrary_turn_start(
    thread_id: str,
    input: Any,
    *,
    warden: WardenClient | None = None,
) -> Any:
    return await _client(warden).arbitrary_turn_start(thread_id, input)


async def arbitrary_turn_steer(
    thread_id: str,
    turn_id: str,
    input: Any,
    *,
    warden: WardenClient | None = None,
) -> Any:
    return await _client(warden).arbitrary_turn_steer(thread_id, turn_id, input)


async def arbitrary_turn_interrupt(
    thread_id: str,
    turn_id: str,
    *,
    warden: WardenClient | None = None,
) -> Any:
    return await _client(warden).arbitrary_turn_interrupt(thread_id, turn_id)


__all__ = [
    "arbitrary_thread_history",
    "arbitrary_thread_snapshot",
    "arbitrary_turn_interrupt",
    "arbitrary_turn_start",
    "arbitrary_turn_steer",
    "current_event",
    "current_thread_history",
    "current_thread_snapshot",
    "interrupt_turn",
    "list_threads",
    "start_turn",
    "steer_turn",
]

"""The code-first Warden hook declaration API."""

from __future__ import annotations

from dataclasses import dataclass
import inspect
from typing import Any, Callable, Iterable, TypeVar, cast

from .actions import WardenAction
from .events import HookEventKind


HookFunction = Callable[..., Any]
_F = TypeVar("_F", bound=HookFunction)
_METADATA_ATTRIBUTE = "__warden_hook__"


@dataclass(frozen=True, slots=True)
class HookMetadata:
    """Registration metadata attached to a decorated hook function."""

    events: tuple[HookEventKind, ...]
    actions: tuple[WardenAction, ...]
    blocking: bool

    def to_dict(self) -> dict[str, Any]:
        return {
            "events": [event.value for event in self.events],
            "actions": [action.value for action in self.actions],
            "blocking": self.blocking,
        }


def hook(
    *,
    on: HookEventKind | str | Iterable[HookEventKind | str],
    actions: WardenAction | str | Iterable[WardenAction | str] = (),
    blocking: bool = False,
) -> Callable[[_F], _F]:
    """Declare the normalized events handled by an ordinary Python function."""

    events = _normalize_events(on)
    selected_actions = _normalize_actions(actions)
    if not isinstance(blocking, bool):
        raise TypeError("@hook(blocking=...) must be a bool")

    def decorate(function: _F) -> _F:
        if not callable(function):
            raise TypeError("@hook can only decorate a callable")
        if hasattr(function, _METADATA_ATTRIBUTE):
            raise ValueError(
                f"{function.__name__} is already registered as a Warden hook"
            )
        setattr(
            function,
            _METADATA_ATTRIBUTE,
            HookMetadata(events=events, actions=selected_actions, blocking=blocking),
        )
        return function

    return decorate


def get_hook_metadata(function: HookFunction) -> HookMetadata | None:
    metadata = getattr(function, _METADATA_ATTRIBUTE, None)
    return cast(HookMetadata | None, metadata)


def find_hook(namespace: dict[str, Any]) -> tuple[HookFunction, HookMetadata]:
    """Return the single decorated hook exported by a loaded hook module."""

    registrations: list[tuple[HookFunction, HookMetadata]] = []
    seen: set[int] = set()
    for candidate in namespace.values():
        if not callable(candidate) or id(candidate) in seen:
            continue
        metadata = get_hook_metadata(candidate)
        if metadata is not None:
            seen.add(id(candidate))
            registrations.append((candidate, metadata))

    if not registrations:
        raise ValueError(
            "hook.py must export exactly one function decorated with @hook"
        )
    if len(registrations) != 1:
        raise ValueError("hook.py exports more than one function decorated with @hook")

    function, metadata = registrations[0]
    try:
        hook_arity(function)
    except TypeError as error:
        raise ValueError(
            "hook function must accept event or event and warden arguments"
        ) from error
    return function, metadata


def hook_arity(function: HookFunction) -> int:
    """Return whether a hook accepts only event or event plus Warden client."""

    signature = inspect.signature(function)
    try:
        signature.bind(object(), object())
        return 2
    except TypeError:
        signature.bind(object())
        return 1


def _normalize_events(
    value: HookEventKind | str | Iterable[HookEventKind | str],
) -> tuple[HookEventKind, ...]:
    if isinstance(value, (HookEventKind, str)):
        values: Iterable[HookEventKind | str] = (value,)
    else:
        values = value

    events: list[HookEventKind] = []
    for item in values:
        event = HookEventKind.parse(item)
        if event is HookEventKind.UNKNOWN_UPSTREAM_EVENT:
            # Explicitly matching unknown messages is supported for advanced hooks.
            pass
        if event not in events:
            events.append(event)
    if not events:
        raise ValueError("@hook(on=...) requires at least one event kind")
    return tuple(events)


def _normalize_actions(
    value: WardenAction | str | Iterable[WardenAction | str],
) -> tuple[WardenAction, ...]:
    if isinstance(value, (WardenAction, str)):
        values: Iterable[WardenAction | str] = (value,)
    else:
        values = value

    actions: list[WardenAction] = []
    for item in values:
        action = WardenAction.parse(item)
        if action not in actions:
            actions.append(action)
    return tuple(actions)

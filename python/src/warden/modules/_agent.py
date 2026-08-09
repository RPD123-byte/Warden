"""Shared host-backed implementation for provider-specific helper modules."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Mapping, Protocol

from ..client import get_current_client
from ..events import HookEvent


class _Requester(Protocol):
    async def request(
        self, method: str, params: Mapping[str, Any] | None = None
    ) -> Any: ...


async def run(
    provider: str,
    event: HookEvent | Mapping[str, Any],
    *,
    prompt: str = "",
    warden: _Requester | None = None,
) -> Any:
    """Request a fresh provider conversation from the Warden host."""

    client = warden or get_current_client()
    return await client.request(
        "agent.run",
        {
            "provider": provider,
            "event": _event_dict(event),
            "prompt": _prompt(prompt),
        },
    )


@dataclass(frozen=True, slots=True)
class AgentSession:
    """A named provider conversation persisted and serialized by the host."""

    provider: str
    name: str
    prompt: str = ""
    warden: _Requester | None = None

    async def send(
        self,
        event: HookEvent | Mapping[str, Any],
        *,
        prompt: str | None = None,
    ) -> Any:
        client = self.warden or get_current_client()
        return await client.request(
            "agent.session.send",
            {
                "provider": self.provider,
                "name": self.name,
                "event": _event_dict(event),
                "prompt": self.prompt if prompt is None else _prompt(prompt),
            },
        )

    async def reset(self) -> Any:
        client = self.warden or get_current_client()
        return await client.request(
            "agent.session.reset", {"provider": self.provider, "name": self.name}
        )

    async def status(self) -> Any:
        client = self.warden or get_current_client()
        return await client.request(
            "agent.session.status", {"provider": self.provider, "name": self.name}
        )


def session(
    provider: str,
    name: str,
    *,
    prompt: str = "",
    warden: _Requester | None = None,
) -> AgentSession:
    if not isinstance(name, str) or not name.strip():
        raise ValueError("persistent agent session name must be a non-empty string")
    return AgentSession(
        provider=provider,
        name=name.strip(),
        prompt=_prompt(prompt),
        warden=warden,
    )


def _event_dict(event: HookEvent | Mapping[str, Any]) -> dict[str, Any]:
    if isinstance(event, HookEvent):
        return event.to_dict()
    if isinstance(event, Mapping):
        return dict(event)
    raise TypeError("agent event must be a HookEvent or mapping")


def _prompt(prompt: str) -> str:
    if not isinstance(prompt, str):
        raise TypeError("prompt must be a string")
    return prompt

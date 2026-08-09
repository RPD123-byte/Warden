"""Host-backed Claude Code helpers for Warden hooks."""

from __future__ import annotations

from typing import Any, Mapping

from ..events import HookEvent
from ._agent import AgentSession, _Requester
from ._agent import run as _run
from ._agent import session as _session


async def run(
    event: HookEvent | Mapping[str, Any],
    *,
    prompt: str = "",
    model: str | None = None,
    warden: _Requester | None = None,
) -> Any:
    """Run fresh Claude inference through the Warden host."""

    return await _run("claude", event, prompt=prompt, model=model, warden=warden)


def session(
    name: str,
    *,
    prompt: str = "",
    model: str | None = None,
    warden: _Requester | None = None,
) -> AgentSession:
    """Reference a named persistent Claude conversation owned by Warden."""

    return _session("claude", name, prompt=prompt, model=model, warden=warden)

"""Public authoring API for code-first Warden hooks."""

from .actions import WardenAction
from .client import (
    Warden,
    WardenClient,
    WardenConfigurationError,
    WardenError,
    WardenProtocolError,
    WardenRemoteError,
    get_current_client,
)
from .events import HookEvent, HookEventKind
from .hooks import HookMetadata, get_hook_metadata, hook
from . import modules

__all__ = [
    "HookEvent",
    "HookEventKind",
    "HookMetadata",
    "Warden",
    "WardenAction",
    "WardenClient",
    "WardenConfigurationError",
    "WardenError",
    "WardenProtocolError",
    "WardenRemoteError",
    "get_current_client",
    "get_hook_metadata",
    "hook",
    "modules",
]

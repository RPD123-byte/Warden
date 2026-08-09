## Why

Codex's native hook bundle is fixed for an already-loaded task, so it cannot provide the live, user-created automation surface this daemon needs. The host needs its own Warden-hook runtime that reacts to the Codex events already ingested by `codex-control`, while using generated Codex skills as mandatory per-message activation markers in the existing prompt UI.

## What Changes

- Add a long-running Rust host daemon that embeds the `codex-control` dependency and routes its authoritative incoming events into Warden hooks.
- Add code-first Warden hooks that can execute arbitrary supported logic, beginning with Python functions whose isolated runtime dependencies are installed and cached by Warden.
- Generate one mandatory Codex marker skill for every Warden hook, attach the generated skill root to each managed app-server connection, and activate the matching hook only for the user turn containing Codex's selected-skill marker representation.
- Normalize relevant Codex messages into stable Warden hook event kinds such as user-prompt submission, pre-tool use, post-tool use, tool failure, completed agent message, and terminal turn events while retaining the exact source event.
- Pass the incoming event to hook code automatically as the in-memory Rust event or its serialized subprocess representation.
- Provide reusable Claude and Codex execution modules with fresh inference by default and an explicit persistent-conversation option.
- Provide reusable access to selected `codex-control` queries and actions through a Warden action interface usable by subprocess hooks and agent modules.
- Hot-reload Warden hook code, dependencies, and generated marker skills without mutating Codex's native hook bundle.

## Capabilities

### New Capabilities

- `warden-hooks`: Code-first hook discovery, mandatory marker-skill activation, turn-scoped execution, normalized event delivery, managed Python dependencies, reusable modules, and hot reload.
- `managed-agent-sessions`: Fresh and persistent Claude or Codex sessions that receive incoming hook events automatically.
- `hook-action-access`: User-selected access from hook executors to the current `codex-control` observation and control actions, including explicitly selected cross-thread operations.

### Modified Capabilities

None.

## Impact

- Creates the host daemon and hook runtime in this repository.
- Adds a Git/Cargo dependency on `RPD123-byte/Warden` (`codex-control`) and builds on its `Arc<SequencedEvent>`, event store, streams, snapshots, and `start`/`steer`/`interrupt` APIs.
- Requires a small dependency integration addition for app-server skill-root attachment if `codex-control` does not expose the necessary generic request through its public `Handle`.
- Adds local Python environment/process management and Claude/Codex CLI subprocess integration.
- Adds generated local Codex skill files, but does not write to or hot-reload Codex native `hooks.json` definitions.

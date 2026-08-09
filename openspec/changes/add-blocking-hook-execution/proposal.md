## Why

Warden currently reacts to app-server lifecycle notifications as a peer observer, so Codex does not wait for a selected hook before continuing. Hooks need a simple execution-mode choice that works across the event model: non-blocking hooks run in the background, while blocking hooks hold Codex at a real native synchronization point until their work finishes.

## What Changes

- Add `blocking: bool = False` to the code-first `@hook(...)` declaration for every Warden event kind.
- Install a small, stable set of Warden-owned synchronous Codex native hooks that forward native hook events to the daemon over its local socket. These are generic bridges; authored Warden logic remains outside Codex `hooks.json` and generated skills remain activation markers only.
- At each native bridge request, resolve the marker-selected hook revisions for that turn, start matching non-blocking hooks without waiting, run matching blocking hooks concurrently, and reply only after all blocking invocations finish or fail within their bounds.
- Correlate native bridge requests with later app-server lifecycle notifications so one logical event is delivered once and the same immutable hook revision remains active for the turn.
- Expose execution semantics honestly for every event kind: barrier-backed events pause Codex; observer-only or already-terminal events still support both scheduling modes, but blocking can only delay Warden's own event finalization because no unfinished Codex operation remains to hold.
- Add exact-hash discovery and trust management for the Warden-generated native bridge bundle through a narrow typed `codex-control` API. Do not enable a global hook-trust bypass.
- Preserve hot reload after bootstrap: a Codex task must load the generic bridge bundle once, but later Warden hook additions and updates require no native bundle change. Surface when a one-time Codex restart is required to load or refresh the bridge bundle.
- Make normal Warden startup the idempotent Codex onboarding flow: create the Warden data layout, install or update the Codex-facing `create-warden-hook` authoring skill, attach the generated marker-skill root, install and exactly trust the generic bridge bundle, and report restart readiness without creating a sample authored hook.

## Capabilities

### New Capabilities

- `blocking-hook-execution`: Universal Warden hook execution modes, generic native synchronization bridges, event correlation, bridge trust/readiness, and blocking/non-blocking dispatch guarantees.

### Modified Capabilities

None. The prior `warden-hooks` capability has not yet been archived into the main spec store; this change composes with its completed change artifacts and introduces the new execution-mode contract separately.

## Impact

- Changes the Warden Python decorator metadata, Rust registry metadata, activation router, event dispatcher, action socket protocol, diagnostics, hook-creation skill, and hook documentation.
- Makes the daemon startup command the single Codex-only installer and reconciler; no separate onboarding command is required.
- Adds a Warden-managed bridge script and narrowly merges identifiable bridge entries into the user's Codex native hook configuration while preserving unrelated native hooks.
- Extends `codex-control` with typed native hook listing and exact-hash trust operations; GUI restart remains owned by its existing explicit `manage_gui` supervision path.
- Requires integration tests against Codex native synchronous and asynchronous hook behavior, mixed blocking modes, marker scoping, duplicate suppression, bridge failure, trust, hot reload, and existing-session readiness.
- Replaces the earlier design assumption that Warden never mutates Codex native hooks. Individual Warden hooks are still never projected into the native bundle; only the fixed generic bridge layer is managed there.

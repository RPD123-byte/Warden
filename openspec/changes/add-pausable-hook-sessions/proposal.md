## Why

Warden markers currently activate a hook for exactly one user turn, so a user who wants continuous supervision must select the same marker on every message. Warden needs an explicit continuous mode that can remain attached to one Codex task while still giving the user simple, selectable pause and resume controls without borrowing Codex Goal Mode or modifying Codex UI internals.

## What Changes

- Add an opt-in continuous activation session for Warden hooks, scoped by hook and source Codex task.
- Preserve the existing primary marker's one-turn behavior; starting continuous monitoring requires an explicit generated `<hook>-start` control marker.
- Generate marker-only Codex skills for `<hook>-start` and `<hook>-stop` for every hook. Generate `<hook>-pause` and `<hook>-resume` only when the prepared hook declares a named persistent Claude or Codex session that retains conversation history between activations.
- Persist running and paused activation state so daemon restarts do not silently change whether a hook is monitoring a task.
- Classify hooks automatically from their existing agent API usage: named persistent Claude/Codex sessions are stateful; fresh agent inference and hooks with no agent session are stateless. Do not add a lifecycle setting to hook configuration.
- Keep stateful paused sessions dormant without deleting persistent Claude or Codex conversation context; resuming continues the same logical provider session.
- Let running sessions use each hook's latest valid published revision for later events while preserving the immutable revision of an invocation already in flight.
- Add collision, conflicting-control, hook-removal, task-lifecycle, and diagnostics behavior so generated controls never overwrite authored hooks or leave invisible active work.
- Enable the bundled `unspecified-decisions` template to be started, paused, resumed, and stopped through these controls while retaining its existing one-turn marker.

## Capabilities

### New Capabilities

- `pausable-hook-sessions`: Continuous per-task Warden-hook activation, generated lifecycle control markers, durable running/paused state, provider-context continuity, and bounded cleanup behavior.

### Modified Capabilities

None. The existing one-turn marker contract remains unchanged; continuous activation is an additional explicit path.

## Impact

- Warden activation routing, marker generation, durable local state, registry reconciliation, runtime diagnostics, onboarding template behavior, and CLI/README documentation.
- The Python hook authoring contract gains no process-lifetime plumbing; control-session behavior is owned by Warden and selected through generated markers.
- `codex-control` remains responsible for event ingestion and task identity but requires no Codex Goal API or native UI changes for this feature.
- Generated skill names always reserve `-start` and `-stop`; `-pause` and `-resume` are reserved only for hooks classified as stateful, wherever those names would collide with another generated or authored Warden marker.

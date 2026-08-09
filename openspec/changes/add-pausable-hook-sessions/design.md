## Context

See [proposal.md](./proposal.md#why) for motivation. Warden currently resolves a generated skill path to a `HookId`, creates an immutable `HookRevision` activation keyed by `(hook, task, turn)`, and deletes that activation at the terminal turn event. The registry publishes one marker directory per valid hook, while `codex-control` already supplies ordered events, task identity, lifecycle observations, and Codex actions.

Continuous control therefore belongs in the Warden host. It must coexist with the existing one-turn router, preserve the exact marker-only skill body, survive daemon restarts, and avoid treating an idle logical session as a live Claude, Codex, Python, or sidecar process.

## Goals / Non-Goals

**Goals:**

- Add a small deterministic state machine for each `(hook, source task)` without adding lifecycle configuration to the hook decorator.
- Derive whether pause/resume applies from the hook's existing named persistent-agent declaration.
- Make marker generation and marker resolution unambiguous even when hook names resemble control names.
- Compose one-turn and continuous activation without duplicate delivery of one logical event.
- Preserve existing task-scoped persistent provider-session identity through pause and resume.
- Restore durable control state before new events are routed and expose bounded lifecycle diagnostics.

**Non-Goals:**

- Reusing or emulating Codex Goal Mode, its progress row, or `thread/goal/*` operations.
- Keeping provider or hook subprocesses resident while a control session is idle.
- Replaying events emitted while a session was paused or while Warden was disconnected.
- Adding `continuous`, `lifetime`, event-to-prompt, or control fields to hook definitions.
- Resetting hook-owned provider context when a continuous activation is stopped. Existing session-reset semantics remain separate.

## Decisions

### 1. Keep one-turn and continuous state as separate concepts

The existing turn activation map remains keyed by `(HookId, task_id, turn_id)` and keeps its immutable `Arc<HookRevision>`. A new `ContinuousSessionStore` is keyed only by `(HookId, task_id)` and stores `Running` or `Paused`, the most recent transition time, and the most recent lifecycle error.

The router forms eligible deliveries from the union of:

1. the existing one-turn activation for the event's task and turn; and
2. a running continuous session for the event's hook and task.

It deduplicates the union by hook plus the existing logical delivery identity. Selecting the primary marker while a continuous session is already running therefore does not invoke the hook twice. Selecting it while the continuous session is paused still creates a normal one-turn activation and leaves the continuous state paused.

This is preferable to adding a lifetime flag to `ActivationRecord`: turn records intentionally pin a revision and expire on a terminal event, while continuous records intentionally cross turns and resolve current metadata repeatedly. Combining them would make both lifecycle rules conditional and fragile.

### 2. Resolve marker intent through a registry-owned marker catalog

Marker names are not parsed by stripping a suffix from arbitrary user text. During registry reconciliation Warden builds a catalog from canonical generated skill paths to a typed intent:

```text
Primary(HookId)
Control(HookId, Start | Stop)
StatefulControl(HookId, Pause | Resume)
```

For every hook `example`, the catalog publishes `example`, `example-start`, and `example-stop`. It also publishes `example-pause` and `example-resume` when the prepared metadata classifies the hook as stateful. All generated skill files use the existing exact marker body; only front matter descriptions differ. Input is still accepted only when Codex supplies a structured skill selection or selected-skill link whose canonical path is under Warden's generated-skill root. Literal slash text remains inert.

Before publishing any candidate marker set, registry reconciliation validates the full namespace. A hook identifier that equals another hook's generated control name is rejected, and an existing marker directory is never overwritten unless the catalog identifies it as Warden-owned for the same intent. Publication/removal of the five-marker bundle is transactional from the router's perspective: the in-memory catalog changes only after filesystem reconciliation succeeds.

An alternative was suffix parsing in `activation.rs`. It is smaller initially but cannot distinguish `foo-pause` as an authored hook from `foo`'s pause control and makes collision behavior depend on discovery order.

### 3. Derive statefulness from named persistent-agent declarations

The Python SDK's `claude.session(...)` and `codex.session(...)` factories register a persistent-agent declaration while the isolated hook module is imported. The worker clears this import-local collector before loading the candidate and includes `persistent_agent_sessions: true` in its handshake when at least one declaration was observed. Rust stores that derived capability in `HookMetadata`.

The supported code-first convention is to construct named persistent sessions at module scope, as the bundled `unspecified-decisions` hook already does. `claude.run(...)`, `codex.run(...)`, ordinary Python hooks, and hooks with no declaration report false. This adds no author-facing lifecycle flag: choosing the persistent agent API is the distinction.

Pause/resume markers and paused durable state are legal only when the current valid revision reports this capability. If a hot revision changes from stateful to stateless, reconciliation removes its pause/resume markers and any paused records; running records remain running and can still be stopped.

Alternatives considered were a new `@hook(stateful=True)` field and source-code AST inspection. The decorator field duplicates information already expressed by the agent API, while AST inspection is unreliable under aliases and helper modules. Import-time registration uses actual Python execution inside the candidate-preparation boundary and matches the existing persistent-session authoring pattern.

### 4. Apply one control transition per hook before routing the control turn

At user-prompt ingestion, Warden collects all marker intents before creating deliveries. Primary intents create one-turn activations independently. Control intents are grouped by hook:

- no control: leave continuous state unchanged;
- one distinct control: apply it atomically;
- repeated copies of the same control: apply it once;
- two or more different controls: apply none and record a conflict diagnostic.

The state transitions are:

| Current | Start | Pause | Resume | Stop |
|---|---|---|---|---|
| Missing | Running | Missing | Missing + diagnostic | Missing |
| Running | Running | Paused | Running | Missing |
| Paused | Running | Paused | Running | Missing |

`Start` is intentionally an idempotent request for running state and `Stop` is available to every hook. `Pause` and `Resume` are accepted only for a stateful current revision; `Resume` is stricter and only succeeds from `Paused`. Stateless sessions therefore have only `Missing` and `Running` states. The mutation is durably committed before normalized events from that user turn are routed. Consequently start and resume include later matching events in the control turn, while pause and stop suppress continuous delivery beginning with that turn. A simultaneous primary marker can still provide its independent one-turn delivery.

This batch-before-routing rule avoids depending on the order in which Codex serializes selected skills or observed/native event copies.

### 5. Resolve the latest valid hook revision at delivery time

A continuous record stores `HookId`, not `Arc<HookRevision>`. For each event, the router asks `HookRegistry::current` for the latest valid revision and tests that revision's event metadata. The resulting `HookDelivery` owns an `Arc<HookRevision>`, so a refresh cannot mutate an invocation already in flight.

If no valid revision exists, no invocation is started and the continuous record receives a visible error. Removal reconciliation then deletes all continuous records for that hook. This retains the registry's existing last-valid-revision behavior for invalid edits while ensuring a truly removed hook cannot remain invisibly active.

### 6. Persist one bounded record per continuous session

`DataPaths` gains a Warden-owned continuous-session state location under the existing Warden root. State is represented as versioned records containing the hook ID, source task ID, status, transition timestamp, and optional bounded diagnostic metadata. Writes use a temporary file plus rename, followed by parent-directory synchronization where supported. Deletion uses the same explicit key and never a broad recursive target.

Startup loads and validates these records after the registry is initialized but before event processing begins. Unknown schema versions and malformed records are quarantined or reported rather than guessed. Records for missing hooks are removed during reconciliation. Ordinary task unload does not alter state; observed task archive or deletion removes all records for that task. Daemon shutdown requires no special transition because every accepted transition is already durable.

The store does not persist delivery-deduplication history across daemon downtime. It relies on `codex-control`'s ordered ingestion and existing recovery boundaries; coverage gaps are reported and missed events are not replayed. Unlike one-turn activations, a coverage gap does not invent a new state transition or silently change a durable running/paused choice.

One file per session is preferred over a single database or monolithic JSON file because expected cardinality is small, writes affect one task/hook pair, and corruption or failed replacement stays localized. File names use a stable digest of the composite key while the validated IDs remain inside the record.

### 7. Reuse existing provider-session keys without keeping processes alive

The invocation path remains unchanged after routing. A persistent Claude or Codex module derives its provider session from the existing task-scoped key, which already separates identical hooks used in different Codex tasks. Pause does not call reset, and resume supplies the same hook/task identity, so the next invocation continues the same provider conversation.

Stop removes only continuous activation state. It does not implicitly delete provider history because activation lifecycle and conversation retention are separate concerns; the existing explicit reset path remains authoritative. Starting again may therefore reuse a named persistent provider conversation if the hook's existing session policy says to do so.

Continuous state itself never holds a child-process handle. Warden continues to spawn or resume provider work only for an eligible event and releases it under existing runtime behavior afterward.

### 8. Keep `codex-control` unchanged

Warden consumes the ordered event stream and task lifecycle already supplied by `codex-control`. No new ingestion loop, cross-task scan, Goal Mode API, environment variable, or native Codex hook-bundle mutation is needed. If task archive/deletion is not currently normalized into Warden lifecycle handling, Warden adds that adapter at its event boundary rather than duplicating transport logic in the dependency.

### 9. Report compact lifecycle health

Daemon health adds running and paused counts plus a bounded, deterministically ordered list of session summaries containing hook ID, task ID, status, most recent transition, and most recent error. Transition conflicts, invalid resume attempts, missing revisions, corrupt state, and cleanup failures are also logged at the time they occur.

This gives the CLI a truthful inspection surface without claiming native Codex UI integration. The marker controls themselves remain the only interaction required for this change.

## Risks / Trade-offs

- **[A marker can become visible in Codex slightly before or after the in-memory catalog refresh]** → Canonical catalog resolution makes an unmatched marker inert, registry reconciliation is serialized, and Codex skill refresh runs only after successful publication.
- **[A crash between an external task archive and local cleanup can leave a stale record]** → Reconcile known task lifecycle on startup/connection and expose stale or cleanup errors in health; never attach a record to a different task ID.
- **[Long-running sessions can accumulate provider conversation context]** → Keep existing provider compaction/reset behavior separate and documented; pause does not pretend to reduce stored provider history.
- **[Stop followed by start may reuse persistent provider context when a user expected a clean conversation]** → Document that stop controls delivery only and retain the existing explicit session-reset mechanism. A future reset marker can be specified independently if needed.
- **[Hot hook revisions can change behavior during one continuous session]** → Resolve only validated revisions, pin each in-flight invocation, and show revision failures without replacing the last valid revision.
- **[A persistent session constructed only inside the hook function cannot influence marker publication during candidate preparation]** → Define module-scope named session declaration as the supported stateful authoring convention, document it, and test it in the SDK and bundled template.
- **[A coverage gap prevents proof that every running-session event was observed]** → Surface the gap, do not replay or synthesize events, and preserve the user's durable running/paused choice.
- **[A large number of active task/hook pairs could expand routing and health work]** → Index sessions by task, cap diagnostic detail, and perform no provider work for idle or paused sessions.

## Migration Plan

1. Add the versioned state store and state-machine tests without enabling control marker publication.
2. Add import-time persistent-agent declaration reporting, registry catalog validation, and marker reconciliation: primary/start/stop for every hook and pause/resume only for stateful hooks.
3. Integrate control processing and continuous routing while retaining all existing one-turn tests unchanged.
4. Add restart, hot-reload, archive/removal, deduplication, provider-continuity, and bounded-health tests.
5. Update the bundled `unspecified-decisions` fixture/template and user documentation, then run a live Codex/Claude smoke test for start, pause, resume, and stop.

Rollback disables control-marker publication and continuous routing, removes generated control markers, and leaves versioned continuous-state records untouched for a later compatible release. The prior primary markers and one-turn behavior continue to work. If operators intentionally abandon the feature, a targeted Warden cleanup command may remove only the continuous-state directory.

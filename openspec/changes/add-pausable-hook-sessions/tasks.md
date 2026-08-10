## 1. Stateful Metadata and Marker Publication

- [x] 1.1 Track module-scope named Claude/Codex session declarations during isolated Python candidate import and report the derived stateful capability in the worker handshake.
- [x] 1.2 Extend Rust hook metadata and backward-compatible handshake parsing with the derived persistent-agent-session capability.
- [x] 1.3 Add typed primary/start/stop and stateful-only pause/resume marker intents keyed by canonical generated skill paths.
- [x] 1.4 Extend registry reconciliation to validate the applicable marker namespace and reject collisions without overwriting existing markers.
- [x] 1.5 Publish primary/start/stop for every hook and pause/resume only for stateful hooks, reconciling markers when statefulness changes while preserving the exact marker body contract.
- [x] 1.6 Add Python and Rust tests for fresh versus persistent agent classification, marker contents, canonical intent resolution, idempotent reconciliation, collision rejection, statefulness changes, and complete marker cleanup.

## 2. Durable Continuous Session State

- [x] 2.1 Add versioned continuous-session records and a composite `(HookId, task_id)` key with running and paused statuses.
- [x] 2.2 Implement atomic create, transition, restore, and targeted delete operations under a dedicated Warden state path.
- [x] 2.3 Implement start/stop for every hook and stateful-only pause/resume, including idempotent operations, strict resume behavior, conflicting-control rejection, transition timestamps, and bounded errors.
- [x] 2.4 Add state-store tests for every transition, malformed or unknown records, atomic restart recovery, and isolation between hooks and tasks.

## 3. Activation Routing

- [x] 3.1 Extend prompt activation processing to collect all marker intents and durably apply valid control transitions before routing normalized events from that turn.
- [x] 3.2 Route matching events through running continuous sessions across later turns while paused and stopped sessions perform no invocation.
- [x] 3.3 Union continuous and one-turn eligibility with hook-level logical-delivery deduplication so a primary marker never duplicates an already-running delivery and still works independently while paused.
- [x] 3.4 Resolve the latest valid hook revision for each continuous delivery while retaining an immutable revision for every in-flight invocation.
- [x] 3.5 Add router tests for stateless start/stop, stateful same-turn start/resume inclusion, same-turn pause/stop suppression, conflicting controls, later markerless turns, paused one-turn invocation, native/observed deduplication, and hot revision replacement.

## 4. Lifecycle, Recovery, and Dependency Seams

- [x] 4.1 Restore validated continuous sessions after registry initialization and before Warden begins processing new Codex events.
- [x] 4.2 Remove sessions when their hook is removed and when `codex-control` reports the source task archived or deleted, without treating ordinary task unload as removal.
- [x] 4.3 Preserve running or paused state across coverage gaps while surfacing missed coverage and never replaying or synthesizing events.
- [x] 4.4 Add contract tests at the `codex-control` event/task-lifecycle seam proving task IDs remain the isolation key and that archive/delete cleanup does not require a second ingestion loop.
- [x] 4.5 Add restart and lifecycle integration tests covering running restore, paused restore, hook removal, task unload/reload, archive/delete, and cross-task isolation.

## 5. Provider Context and Runtime Behavior

- [x] 5.1 Verify continuous delivery reuses the existing task-scoped persistent Claude/Codex session key without storing process handles in continuous state.
- [x] 5.2 Ensure pause/resume never resets provider history, stop affects delivery only, and no provider, Python, or sidecar process is started for paused or idle sessions.
- [x] 5.3 Add provider integration tests proving same-task Claude context continues after pause/resume, another task gets a separate conversation, and paused events are not replayed.

## 6. Diagnostics, Template, and Documentation

- [x] 6.1 Extend Warden health with bounded, deterministically ordered running/paused counts and session summaries including recent transition or error.
- [x] 6.2 Surface control conflicts, invalid resume attempts, missing revisions, corrupt durable state, and cleanup failures in logs and health diagnostics.
- [x] 6.3 Verify the bundled `unspecified-decisions` module-scope Claude session is classified stateful so onboarding publishes all four lifecycle controls without adding hook lifecycle configuration.
- [x] 6.4 Update CLI help and README guidance to explain one-turn versus continuous activation, task scoping, provider-context retention, stop versus reset, and the absence of native Goal Mode UI.

## 7. End-to-End Verification

- [x] 7.1 Run Rust formatting, linting, unit tests, integration tests, and the existing Python hook-runtime test suite.
- [x] 7.2 Run a live Codex/Claude smoke test that starts `unspecified-decisions`, observes a markerless later turn, pauses it, confirms no inference occurs, resumes the same Claude conversation, and stops it.
- [x] 7.3 Restart Warden during both running and paused states and verify restored behavior, bounded health output, no cross-task activation, and no idle provider subprocess.
- [x] 7.4 Accept registry-validated leading `$` and `/` marker commands when Codex omits a structured skill attachment, with router and end-to-end coverage.

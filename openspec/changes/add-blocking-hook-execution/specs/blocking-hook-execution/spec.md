## Purpose

Provide one consistent blocking or non-blocking execution choice for every marker-activated Warden hook while using Codex native synchronization points wherever Codex can actually be held.

## ADDED Requirements

### Requirement: Every hook declares one execution mode
The system SHALL allow every code-first Warden hook to declare `blocking=True` or `blocking=False` in its hook registration metadata. The default SHALL be `blocking=False`, and execution mode SHALL apply to every event kind selected by that hook without requiring per-event process configuration.

#### Scenario: Hook omits execution mode
- **WHEN** a valid hook does not declare `blocking`
- **THEN** Warden publishes and invokes it as a non-blocking hook

#### Scenario: Hook declares blocking execution
- **WHEN** a valid hook declares `blocking=True`
- **THEN** Warden preserves that mode in the immutable published hook revision and applies it to every matching invocation

### Requirement: Non-blocking hooks never hold event progress
The system SHALL start matching non-blocking hook invocations without awaiting their completion before acknowledging a native bridge request or continuing observer-event processing. Their failures and timeouts SHALL remain observable and isolated after the originating event progresses.

#### Scenario: Slow non-blocking hook runs at a native barrier
- **WHEN** a selected non-blocking hook takes longer than the native event operation
- **THEN** Warden acknowledges the native bridge after successfully scheduling the invocation and Codex continues without waiting for that invocation to finish

### Requirement: Blocking hooks hold barrier-backed Codex events
For a Warden event backed by a Codex native synchronous hook point, the system SHALL not acknowledge the native bridge until every matching selected blocking hook has completed, failed, or reached its bounded timeout. Codex SHALL remain held at that native synchronization point while the bridge request is outstanding.

#### Scenario: Blocking user-prompt hook
- **WHEN** a selected blocking hook matches the user-prompt event
- **THEN** Codex waits at `UserPromptSubmit` until that Warden invocation finishes within its bound

#### Scenario: Blocking pre-tool hook
- **WHEN** a selected blocking hook matches a tool-start event for which Codex invokes `PreToolUse`
- **THEN** Codex does not execute the tool until that Warden invocation finishes within its bound

#### Scenario: Blocking successful post-tool hook
- **WHEN** a selected blocking hook matches a successful tool result backed by `PostToolUse`
- **THEN** Codex waits before continuing beyond the tool result until that Warden invocation finishes within its bound

### Requirement: Mixed execution modes are dispatched concurrently
The system SHALL partition all matching invocations for one logical event by their published execution mode. It SHALL start non-blocking invocations independently, run blocking invocations concurrently rather than serially, and release the native barrier after all blocking invocations reach a terminal invocation outcome.

#### Scenario: One event matches blocking and non-blocking hooks
- **WHEN** one marker-activated event matches two blocking hooks and one non-blocking hook
- **THEN** Warden starts all three, waits concurrently for only the two blocking invocations, and releases Codex when both blocking invocations terminate

### Requirement: Native bridges remain generic and stable
The system SHALL manage a fixed generic bridge entry for each supported native synchronization point and SHALL NOT generate a Codex native hook definition for an individual authored Warden hook. Each bridge SHALL forward the native event payload and correlation identifiers to the local Warden daemon and produce no hook-specific behavior itself.

#### Scenario: New Warden hook is published
- **WHEN** a new valid blocking or non-blocking Warden hook is hot-published after a Codex task loaded the generic bridge bundle
- **THEN** the task can invoke that hook on a later marker-selected turn without changing its native hook bundle

#### Scenario: Unrelated native hooks already exist
- **WHEN** Warden installs or updates its identifiable bridge entries in a native hook configuration containing unrelated user hooks
- **THEN** it preserves the unrelated hook definitions and their relative contents

### Requirement: Bridge activation remains marker-scoped per user message
The system SHALL execute an authored Warden hook through a native bridge only when the current user message contains that hook's valid generated marker skill. The resulting activation SHALL remain bound to the selected hook revision, thread, and turn and SHALL not carry into a later user message.

#### Scenario: Marker selects a blocking hook
- **WHEN** the current user message selects a blocking Warden marker skill
- **THEN** the generic bridge resolves and runs that selected revision for matching events in only that turn

#### Scenario: Marker is omitted from the next message
- **WHEN** the next user message omits the marker
- **THEN** neither blocking nor non-blocking invocations of that hook run for the next turn

### Requirement: Native and observed representations are deduplicated
The system SHALL correlate a native bridge request and later app-server lifecycle notification that represent the same logical event. A hook revision SHALL receive that logical event once, while later distinct events in the same turn remain deliverable in authoritative order.

#### Scenario: User prompt appears through both paths
- **WHEN** Warden receives a native `UserPromptSubmit` bridge request and later observes the corresponding user-prompt lifecycle event
- **THEN** each selected hook receives one user-prompt invocation rather than two

#### Scenario: Replayed observer notification follows native delivery
- **WHEN** lifecycle replay includes an event already delivered through a native bridge
- **THEN** Warden suppresses only the duplicate logical delivery and retains the activation for later matching events

### Requirement: Observer-only events support both modes without false pause guarantees
The system SHALL accept blocking and non-blocking hooks for every Warden event kind. For an event with no corresponding unfinished Codex native synchronization point, a blocking invocation SHALL delay Warden's own ordered processing and activation finalization, while diagnostics and documentation SHALL state that it cannot pause an operation that has already completed. Initial observer-only cases SHALL include failed tool completion, failed or interrupted turns, and unknown upstream events unless the installed Codex version exposes an exact native barrier.

#### Scenario: Blocking hook receives an interrupted turn
- **WHEN** a blocking hook matches `TURN_INTERRUPTED`
- **THEN** Warden awaits and records the invocation before finalizing its activation, without claiming that the already-interrupted Codex turn was paused

#### Scenario: Non-blocking hook receives an observer-only event
- **WHEN** a non-blocking hook matches an observer-only event
- **THEN** Warden schedules the invocation and continues its event loop without waiting

### Requirement: Bridge failures are bounded and fail open
The generic native bridge SHALL use bounded request and invocation deadlines. If Warden is unavailable, the bridge protocol fails, or a blocking invocation crashes or times out, the system SHALL surface the failure and release Codex rather than indefinitely freezing the task. A hook timeout counts as terminal completion for barrier release and SHALL cancel or revoke the timed-out invocation using existing runtime behavior.

#### Scenario: Daemon is unavailable
- **WHEN** Codex invokes a Warden bridge while the daemon socket cannot be reached
- **THEN** the bridge reports the failure through native hook diagnostics and exits successfully so Codex continues

#### Scenario: Blocking agent invocation times out
- **WHEN** a blocking hook awaits an agent beyond the configured invocation bound
- **THEN** Warden records the timeout, revokes the invocation, responds to the bridge, and allows Codex to continue

### Requirement: Bridge trust is exact and readiness is observable
The system SHALL discover its generated bridge hooks, trust only their exact current hashes through a typed Codex configuration operation, and SHALL NOT enable a global native-hook trust bypass. It SHALL report whether the bridge bundle is absent, untrusted, modified, loaded, or requires a Codex task restart before blocking guarantees are active.

#### Scenario: Generated bridge is initially untrusted
- **WHEN** Codex discovers a bridge entry generated by the running Warden installation
- **THEN** Warden writes trust for that bridge's exact discovered key and current hash without trusting unrelated hooks

#### Scenario: Existing task predates bridge installation
- **WHEN** a running Codex task loaded its native hook bundle before Warden installed the generic bridges
- **THEN** Warden reports that blocking readiness requires a task or managed GUI restart and does not claim the task is protected by a native barrier

#### Scenario: Bridge-ready task receives a hot hook update
- **WHEN** a task has loaded the trusted generic bridges and an authored hook revision changes
- **THEN** the next marker-selected invocation uses the new valid revision without another Codex restart

### Requirement: Daemon startup onboards Codex idempotently
The Warden daemon startup command SHALL be the Codex-only installation and reconciliation flow. Before accepting hook events it SHALL create the Warden data layout, install or update the Codex-facing `create-warden-hook` authoring skill from Warden-owned content, attach the generated marker-skill root, install the generic native bridge bundle, and attempt exact-hash trust. Repeated startup SHALL preserve authored hooks, generated markers, unrelated Codex skills and native hooks, and SHALL NOT create a sample authored hook.

#### Scenario: First Warden startup
- **WHEN** a user starts Warden against a Codex installation with no prior Warden state
- **THEN** the required directories, authoring skill, generic bridges, marker-skill root, and exact trust are reconciled automatically and readiness reports whether a Codex restart is required

#### Scenario: Repeated Warden startup
- **WHEN** startup reconciliation runs after Warden is already installed
- **THEN** matching artifacts remain unchanged, outdated Warden-owned artifacts are updated atomically, and unrelated user configuration remains untouched

#### Scenario: Normal attach discovers restart-required bridges
- **WHEN** normal startup installs or updates a bridge bundle that an existing Codex task has not loaded
- **THEN** Warden reports restart-required without quitting or restarting Codex Desktop

#### Scenario: Explicit managed startup installs before launch
- **WHEN** Warden starts with explicit Codex GUI ownership enabled
- **THEN** installation completes before the managed Codex process is launched so newly created tasks load the current bridges

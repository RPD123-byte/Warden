## Purpose

Provides explicit continuous Warden-hook activation with generated start and stop controls, plus pause and resume for stateful agent hooks, while preserving the existing one-turn marker contract.

## ADDED Requirements

### Requirement: Continuous activation is explicit and task-scoped
The system SHALL support at most one continuous activation session for each `(hook, source Codex task)` pair. The existing primary hook marker SHALL remain one-turn-only and SHALL NOT create, resume, pause, or stop a continuous session. A generated `<hook>-start` marker SHALL create or activate the continuous session only for the Codex task whose user message contains that marker.

#### Scenario: Primary marker retains existing behavior
- **WHEN** a user selects the primary hook marker without a continuous control marker
- **THEN** Warden activates that hook only for the submitted user turn
- **AND** Warden does not create or mutate a continuous activation session

#### Scenario: Start marker begins continuous monitoring
- **WHEN** a user selects `<hook>-start` in a Codex task with no continuous session for that hook
- **THEN** Warden records that hook as running for that source task
- **AND** matching events from the control turn and later turns are eligible for delivery without repeating a marker

#### Scenario: Same hook is started in another task
- **WHEN** `<hook>-start` is selected in two different Codex tasks
- **THEN** Warden creates independent continuous activation sessions keyed by their distinct task identifiers

### Requirement: Generated markers match hook statefulness
For each published hook, the system SHALL generate selectable marker skills named `<hook>-start` and `<hook>-stop` in addition to the existing primary marker. The system SHALL additionally generate `<hook>-pause` and `<hook>-resume` only when the prepared hook is classified as stateful because it declares a named persistent Claude or Codex session that retains conversation history between activations. Hooks using only fresh Claude or Codex inference, and hooks using no agent, SHALL be classified as stateless. This classification SHALL be derived from the existing agent-session declaration rather than a new hook lifecycle configuration field. Every generated control skill SHALL contain exactly `This skill is an activation marker for the local Warden service. Ignore`, and its path and generated metadata SHALL identify the target hook and control operation without placing executable hook logic in the skill.

#### Scenario: Stateful agent hook publishes all controls
- **WHEN** Warden publishes a valid hook that declares a named persistent Claude or Codex session
- **THEN** the primary, start, stop, pause, and resume markers are discoverable through Codex skill selection
- **AND** no generated marker contains hook execution logic

#### Scenario: Stateless hook publishes only start and stop
- **WHEN** Warden publishes a valid hook that uses only fresh agent inference or no agent
- **THEN** its primary, start, and stop markers are discoverable through Codex skill selection
- **AND** no pause or resume marker is generated for that hook

#### Scenario: Hook changes statefulness
- **WHEN** a new valid revision changes from stateless to stateful or from stateful to stateless
- **THEN** Warden reconciles pause and resume markers to match the latest valid prepared revision
- **AND** an existing paused session is removed if the hook becomes stateless

#### Scenario: Hook or control name would collide
- **WHEN** an authored hook identifier would collide with another hook's generated start, stop, or applicable stateful pause/resume marker name
- **THEN** Warden rejects the conflicting publication with a diagnostic
- **AND** Warden does not overwrite an existing authored or generated marker

#### Scenario: Hook is removed
- **WHEN** an authored hook directory is removed from discovery
- **THEN** Warden removes that hook's primary and control markers
- **AND** Warden stops and removes its continuous activation sessions so re-adding the hook requires an explicit new start

### Requirement: Available lifecycle transitions are deterministic
The system SHALL process continuous control markers before routing ordinary hook deliveries for the same user turn. Start and stop SHALL apply to both stateful and stateless hooks. For stateful hooks only, pause SHALL retain the session in a dormant state and resume SHALL reactivate an existing paused session. Repeating an already-satisfied available control outcome SHALL be idempotent. Resume SHALL NOT implicitly create a session that was never started or was stopped.

#### Scenario: Stateless hook is started and stopped
- **WHEN** a user starts a stateless hook and later selects its stop marker
- **THEN** matching events are continuously delivered between those controls
- **AND** the hook has no paused lifecycle state

#### Scenario: Running session is paused
- **WHEN** a user selects `<hook>-pause` for a running continuous session
- **THEN** Warden records the session as paused before routing later events from that control turn
- **AND** the hook receives no continuous deliveries until it is resumed

#### Scenario: Paused session is resumed
- **WHEN** a user selects `<hook>-resume` for a paused continuous session
- **THEN** Warden records the session as running before routing later events from that control turn
- **AND** later matching events are delivered normally

#### Scenario: Session is stopped
- **WHEN** a user selects `<hook>-stop` for a running or paused continuous session
- **THEN** Warden removes the continuous activation before routing later events from that control turn
- **AND** later turns are not monitored unless the user explicitly starts another continuous session or selects the primary one-turn marker

#### Scenario: Resume has no paused session
- **WHEN** a user selects `<hook>-resume` after the session was stopped or before it was started
- **THEN** Warden leaves activation state unchanged
- **AND** Warden surfaces that there was no paused session to resume

#### Scenario: One prompt contains conflicting controls
- **WHEN** one user message selects more than one different continuous control for the same hook
- **THEN** Warden applies none of those controls
- **AND** Warden reports the conflict instead of choosing an order implicitly

### Requirement: Running sessions route matching events across turns
While a continuous session is running, the system SHALL deliver every event matching the hook's current valid metadata for that source task, including events from later turns that contain no marker. A paused or stopped continuous session SHALL produce no continuous deliveries. Native and observed copies of one logical event SHALL remain deduplicated under the existing delivery identity rules.

#### Scenario: Later turn omits all markers
- **WHEN** a hook has a running continuous session and a later turn emits a matching event without any Warden marker
- **THEN** Warden delivers that event to the hook exactly once

#### Scenario: Paused session observes later activity
- **WHEN** a continuous session is paused and its source task emits otherwise matching events
- **THEN** Warden performs no invocation and starts no agent inference for those events

#### Scenario: Primary marker is selected while continuous session is paused
- **WHEN** the user selects the primary one-turn marker while that hook's continuous session is paused
- **THEN** Warden invokes the hook for the selected turn under the one-turn contract
- **AND** the continuous session remains paused afterward

#### Scenario: Hook revision changes while running
- **WHEN** a later valid hook revision is published after a continuous session starts
- **THEN** later eligible events use the latest valid revision
- **AND** an invocation already in flight completes against the immutable revision with which it started

### Requirement: Continuous state survives ordinary daemon and task unloading
The system SHALL persist running continuous activation state for every hook and paused state only for stateful hooks, atomically under Warden-managed local state. A Warden daemon restart or ordinary Codex task unload/reload SHALL restore the same activation status without requiring another start marker. Archiving or deleting the source Codex task SHALL remove its continuous activation sessions.

#### Scenario: Daemon restarts while session is running
- **WHEN** Warden restarts after durably recording a running continuous session
- **THEN** Warden restores that session as running before routing new matching events

#### Scenario: Daemon restarts while session is paused
- **WHEN** Warden restarts after durably recording a paused continuous session
- **THEN** Warden restores that session as paused
- **AND** it performs no hook invocation until an explicit resume

#### Scenario: Source task is archived or deleted
- **WHEN** Warden observes that a source Codex task was archived or deleted
- **THEN** Warden removes every continuous activation session scoped to that task
- **AND** another task never inherits those sessions

### Requirement: Stateful classification and pausing preserve provider context
The prepared metadata SHALL report whether the hook declares at least one named persistent Claude or Codex session. Pausing or resuming such a continuous activation SHALL NOT reset its persistent conversations. A resumed invocation in the same source task SHALL resolve the same persistent provider-session key it used before pausing. Merely keeping a continuous activation running or paused SHALL NOT keep a Claude, Codex, Python, or sidecar process alive when no event is being invoked.

#### Scenario: Fresh agent hook remains stateless
- **WHEN** a hook calls fresh Claude or Codex inference without declaring a named persistent session
- **THEN** prepared metadata classifies it as stateless
- **AND** Warden generates no pause or resume controls

#### Scenario: Persistent Claude monitor resumes after pause
- **WHEN** a continuous hook uses a named persistent Claude session, processes an event, is paused, and is later resumed in the same Codex task
- **THEN** its next invocation resumes the same Claude conversation identifier and earlier review context

#### Scenario: Different source task starts the same hook
- **WHEN** another Codex task starts the same continuous Claude-backed hook
- **THEN** that task receives a different persistent Claude conversation

#### Scenario: Session remains idle
- **WHEN** a continuous activation is running or paused but no matching event is being processed
- **THEN** Warden retains only durable and in-memory control state
- **AND** it does not retain a provider or hook subprocess solely to represent that status

### Requirement: Lifecycle state is observable without Codex Goal Mode
The system SHALL expose running continuous sessions and stateful paused sessions, their hook and source-task identities, and their most recent transition or error through Warden diagnostics. Continuous control markers SHALL NOT call Codex `thread/goal/*` operations, create a Codex goal, change Codex goal accounting, or claim to render the native Codex goal progress row.

#### Scenario: Health is inspected
- **WHEN** a user requests Warden health while continuous sessions exist
- **THEN** diagnostics report bounded counts and identities for running and paused sessions
- **AND** the user can distinguish an inactive hook from a paused one

#### Scenario: Continuous session starts
- **WHEN** Warden processes `<hook>-start`
- **THEN** no Codex goal is created or modified
- **AND** Codex does not begin autonomous Goal Mode work because of the Warden control

### Requirement: Bundled decision monitor supports continuous controls
The bundled `unspecified-decisions` hook SHALL publish all continuous control markers while retaining its existing `unspecified-decisions` one-turn marker. Its persistent Claude Sonnet conversation SHALL continue across running events and pause/resume transitions within one source Codex task.

#### Scenario: Decision monitor is started continuously
- **WHEN** the user selects `unspecified-decisions-start`
- **THEN** the decision monitor reviews configured events across subsequent turns until paused or stopped

#### Scenario: Decision monitor is paused and resumed
- **WHEN** the user pauses `unspecified-decisions`, performs unmonitored turns, and later resumes it
- **THEN** no paused-turn events are replayed retroactively
- **AND** later reviews resume the same task-specific Claude conversation with its pre-pause context

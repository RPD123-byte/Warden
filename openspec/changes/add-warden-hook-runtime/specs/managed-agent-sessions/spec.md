## Purpose

Provide reusable Claude and Codex execution modules so a Warden hook can run fresh inference or deliberately retain conversation context across selected hook events and later activations.

## ADDED Requirements

### Requirement: Hooks can invoke Claude or Codex through reusable modules
The system SHALL provide reusable hook modules for locally authenticated Claude Code and Codex CLI execution. The modules SHALL manage subprocess startup, structured input and output, session identifiers, failure reporting, and cleanup without requiring each hook to reproduce that plumbing.

#### Scenario: Python hook invokes Claude
- **WHEN** hook code passes its incoming event and a prompt to the Claude module
- **THEN** Warden runs Claude with the local user's configured authentication and returns the structured result to the hook

#### Scenario: Python hook invokes Codex
- **WHEN** hook code passes its incoming event and a prompt to the Codex module
- **THEN** Warden runs Codex non-interactively and returns the structured result to the hook

### Requirement: Fresh inference is the default
The system SHALL use a fresh agent conversation for each module invocation unless the hook explicitly creates or references a persistent session. Routine subprocess lifetime choices SHALL remain internal runtime behavior rather than required hook configuration.

#### Scenario: Hook uses one-shot agent call
- **WHEN** hook code calls the ordinary agent-run operation twice for two events
- **THEN** the second inference receives no conversational state from the first inference unless that state is included in the second event or prompt

### Requirement: Hooks can explicitly retain agent context
The system SHALL allow hook code to create a named persistent Claude or Codex session and send later active-turn events to the same provider conversation. Warden SHALL preserve or resume that conversation across subprocess restarts when supported by the provider CLI.

#### Scenario: Persistent monitor receives several events
- **WHEN** one active hook invocation sends a user prompt, tool result, and completed agent message to the same persistent session
- **THEN** the agent receives them in order with the conversation context produced by earlier sends

#### Scenario: Persistent session is reused after a dormant turn
- **WHEN** a hook's persistent session exists, a later Codex turn does not activate the hook, and a subsequent turn activates it again
- **THEN** no event from the inactive turn is sent and the subsequent active turn resumes the existing agent conversation

### Requirement: Agent modules deliver the event as the user message
The system SHALL serialize the incoming hook event as the agent module's user message by default and SHALL include any hook-supplied prompt as standing guidance or accompanying instruction. Hook authors SHALL not need to configure an event-to-prompt transformation for ordinary use.

#### Scenario: Agent module receives a post-tool event
- **WHEN** a hook forwards its `PostToolUse` event to an agent session
- **THEN** the next agent input contains that event without additional mapping code from the hook author

### Requirement: Persistent sessions serialize their work
The system SHALL preserve input order for a persistent agent session and SHALL prevent simultaneous sends from corrupting one provider conversation. A crashed or unrecoverable session SHALL fail observably without blocking unrelated sessions.

#### Scenario: Two matching events arrive close together
- **WHEN** an active hook sends two events to one persistent session before the first inference finishes
- **THEN** Warden queues them and delivers them in source-sequence order

#### Scenario: Provider process crashes
- **WHEN** a provider subprocess exits unexpectedly
- **THEN** Warden reports the affected send as failed and either resumes the durable conversation on a later send or marks the session unavailable with a clear reason

### Requirement: Agent sessions receive only events from active hook invocations
The system SHALL route events to an agent session only through an active Warden hook invocation, regardless of whether the provider conversation persists longer than that invocation.

#### Scenario: Session remains alive after activation expires
- **WHEN** a persistent agent session remains available after its source Codex turn completes
- **THEN** Warden keeps the session dormant until a later valid activation sends another event

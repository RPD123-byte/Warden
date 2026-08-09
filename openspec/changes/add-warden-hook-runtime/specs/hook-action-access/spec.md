## Purpose

Allow hook executors and their agent sessions to use explicitly selected Warden observation and Codex control actions without requiring direct access to the in-memory Rust control handle.

## ADDED Requirements

### Requirement: Warden exposes its actions through a local subprocess interface
The system SHALL provide a Warden CLI that connects to the running daemon and projects supported `codex-control` queries and actions as callable commands. The daemon SHALL remain the owner of the in-memory `codex-control` handle and SHALL return its explicit confirmed, rejected, or outcome-unknown results without converting ambiguity into success.

#### Scenario: Agent interrupts its current turn
- **WHEN** an authorized hook subprocess calls the current-turn interrupt command
- **THEN** the daemon targets the invocation's bound thread and turn and returns the underlying Warden action outcome

#### Scenario: Written action has unknown outcome
- **WHEN** `codex-control` reports that an action may have been written but cannot be uniquely confirmed
- **THEN** the CLI reports an outcome-unknown result and does not silently retry the action

### Requirement: Hook creation selects available Warden actions
The Warden hook-creation workflow SHALL ask the user which Warden actions an agent-backed hook may use and SHALL present a multiselect that distinguishes current-event/current-thread actions from thread listing and arbitrary cross-thread actions.

#### Scenario: User selects current-turn actions only
- **WHEN** the user grants current-thread reading, steering, and interruption but does not select thread listing or arbitrary-thread actions
- **THEN** the created hook exposes only the selected current-scope Warden commands to its agent module

#### Scenario: User deliberately selects thread listing
- **WHEN** the user selects thread listing during hook creation
- **THEN** the created hook can expose the thread-list command to its agent module

### Requirement: Warden validates selected action access
The daemon SHALL associate each subprocess invocation with its hook's selected Warden actions and target scope and SHALL reject Warden CLI requests outside that selection. This validation SHALL protect Warden actions only and SHALL not claim to sandbox the subprocess's filesystem, network, or ordinary shell access.

#### Scenario: Hook calls an unselected Warden command
- **WHEN** a hook subprocess attempts to invoke a Warden action that was not selected for that hook
- **THEN** the daemon returns an access-denied result without executing the action

#### Scenario: Current-scope hook supplies another thread identifier
- **WHEN** a hook limited to the current thread attempts to target a different thread
- **THEN** the daemon rejects the request even if the thread identifier is valid

### Requirement: Initial action catalog covers existing Warden primitives
The initial action catalog SHALL include current event access, current thread snapshot or retained-event queries, turn start, turn steer, and turn interrupt. It SHALL offer thread listing and arbitrary-thread variants only as explicitly selectable cross-thread actions.

#### Scenario: Hook reads current context
- **WHEN** a hook with current-context read access requests its triggering event or source-thread snapshot
- **THEN** Warden returns only the context bound to that invocation

#### Scenario: Hook has no action access
- **WHEN** a hook is created without any selected Warden actions
- **THEN** its arbitrary Python or agent inference may still run but Warden control and query commands are unavailable to it

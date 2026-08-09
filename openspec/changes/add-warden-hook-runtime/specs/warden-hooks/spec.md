## Purpose

Provide a simple, code-first automation surface that runs user-defined Warden logic against live Codex events for exactly the turn in which its mandatory marker skill is selected.

## ADDED Requirements

### Requirement: Every Warden hook has a mandatory marker skill
The system SHALL generate one Codex skill for every discovered Warden hook, derive the skill identity from the hook identity, and make the generated skill available through the managed app-server skill roots. The generated `SKILL.md` body SHALL be exactly `This skill is an activation marker for the local Warden service. Ignore` and SHALL contain no hook implementation logic.

#### Scenario: Newly created hook becomes selectable
- **WHEN** a valid Warden hook is added while the daemon and a Codex app-server are running
- **THEN** the system generates its marker skill and refreshes skill discovery so the hook can be selected without writing a Codex native hook definition

#### Scenario: App-server reconnects
- **WHEN** Warden reconnects to or begins managing an app-server
- **THEN** the system attaches the generated marker-skill root to that app-server before relying on marker-skill activation

### Requirement: Marker activation is scoped to one user turn
The system SHALL activate a Warden hook only when the starting input of a Codex turn contains Codex's selected-skill representation for that hook under Warden's generated skill root. The resolver SHALL support structured skill input when present and the installed Codex Desktop app-server's leading `[$name](absolute/SKILL.md)` marker link, and SHALL canonicalize the selected path before activation. The activation SHALL apply to matching events from that turn and SHALL expire when the turn becomes terminal.

#### Scenario: Hook is selected for a message
- **WHEN** a user starts a turn with a Warden marker skill
- **THEN** the matching hook receives its configured events from that turn

#### Scenario: Hook is not repeated
- **WHEN** the next user turn does not contain the marker skill
- **THEN** the hook receives no events from the next turn even if it was active in the previous turn

#### Scenario: Different marker skill is selected
- **WHEN** a turn contains a marker skill belonging to another Warden hook
- **THEN** the system does not activate this hook for that turn

### Requirement: Hooks are code-first and configuration-light
The system SHALL support a Warden hook as an ordinary Python function in a conventionally named hook directory without requiring a YAML hook definition. The hook SHALL declare the normalized event kinds it handles through the Warden Python interface, and routine process-management or event-conversion settings SHALL not be required from the hook author.

#### Scenario: Minimal Python hook
- **WHEN** a hook directory contains a valid `hook.py` using the Warden hook interface and no dependency manifest
- **THEN** Warden discovers it, generates its marker skill, and invokes its function for matching active-turn events

### Requirement: Warden manages Python dependencies
The system SHALL create and cache an isolated Python runtime for a hook that declares dependencies in a supported dependency manifest. The system SHALL rebuild the cached runtime when the dependency declaration changes and SHALL report dependency setup failures without terminating the daemon or unrelated hooks.

#### Scenario: Hook declares a third-party dependency
- **WHEN** a hook is created with a supported Python dependency manifest
- **THEN** Warden prepares an isolated runtime containing that dependency before marking the hook revision ready

#### Scenario: Dependency installation fails
- **WHEN** Warden cannot prepare a hook's declared dependencies
- **THEN** that hook revision remains unavailable, the last valid revision remains usable when one exists, and the failure is observable

### Requirement: Incoming events are passed automatically
The system SHALL invoke hook code with the exact authoritative `codex-control` source event and a stable normalized hook event kind. In-process Rust consumers SHALL be able to retain the shared event object, while subprocess hooks SHALL receive a faithful serialized representation containing the source sequence, thread, turn, item when available, normalized kind, normalized payload, raw method, and raw payload.

#### Scenario: Python hook receives a tool result
- **WHEN** an active hook matches a completed tool-use event
- **THEN** its Python function receives that event as its `event` argument without declaring an event-to-prompt transform

#### Scenario: Hook needs an unnormalized field
- **WHEN** a hook reads a field that Warden does not expose in its normalized payload
- **THEN** the hook can access the preserved raw method and raw payload from the same event

### Requirement: Warden exposes stable hook event kinds
The system SHALL normalize relevant incoming Codex messages into at least `UserPromptSubmitted`, `TurnStarted`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `AgentMessageCompleted`, `TurnCompleted`, `TurnFailed`, and `TurnInterrupted`. Unknown upstream messages SHALL remain representable without being misclassified as a known event.

#### Scenario: Tool item starts
- **WHEN** Warden observes an `item/started` event whose item represents a tool execution
- **THEN** it emits `PreToolUse` to matching active hooks and preserves that this is an observational start event rather than a blocking interception point

#### Scenario: Tool item fails
- **WHEN** Warden observes terminal tool-item data indicating failure
- **THEN** it emits `PostToolUseFailure` rather than successful `PostToolUse`

#### Scenario: Agent message completes
- **WHEN** Warden observes a completed agent-message item
- **THEN** it emits `AgentMessageCompleted` independently of the later terminal turn event

### Requirement: Hooks can import reusable Warden modules
The system SHALL provide an importable Warden Python package containing reusable modules for agent execution, Warden actions, and future shared hook behavior. A hook SHALL be able to combine those modules with arbitrary Python logic in the same function.

#### Scenario: Hook combines local logic with an agent module
- **WHEN** a Python hook imports a Warden agent module, filters the incoming event, and invokes the module conditionally
- **THEN** Warden executes the hook logic in its managed runtime and the module receives the selected event

### Requirement: Hook updates are applied without native hook mutation
The system SHALL watch Warden hook source, supported dependency manifests, and generated marker skills. A valid update SHALL become a new hook revision for subsequent activations; an in-flight activation SHALL retain its starting revision. The system SHALL not implement hot reload by editing Codex's native hook bundle.

#### Scenario: Hook changes between user turns
- **WHEN** a hook implementation is updated after one activation completes
- **THEN** the next activation uses the new valid revision without restarting the Codex task

#### Scenario: Hook changes during an active turn
- **WHEN** a hook implementation changes while an invocation is active
- **THEN** the active invocation continues consistently on its original revision and a later invocation uses the new revision

### Requirement: Hook failures are isolated
The system SHALL contain hook exceptions, process crashes, invalid output, and timeouts so they do not terminate the daemon or prevent unrelated hooks from running. Failures SHALL be associated with the hook revision, source event, and invocation.

#### Scenario: Python hook raises an exception
- **WHEN** a hook function raises an unhandled exception
- **THEN** Warden records the failure for that invocation and continues processing other hooks and Codex events

### Requirement: Warden provides a hook-creation skill
The system SHALL provide a reusable Codex skill that guides an agent to create or update code-first Warden hooks, including event selection, optional Python dependencies, optional agent-session use, and Warden-action selection when an agent is used.

#### Scenario: User asks Codex to create a hook
- **WHEN** the user invokes the Warden hook-creation skill and describes the desired behavior
- **THEN** the skill guides Codex to create the hook in the Warden hook root using the minimal supported files and defaults

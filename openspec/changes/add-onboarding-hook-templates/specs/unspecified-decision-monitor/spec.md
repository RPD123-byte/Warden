## Purpose

Defines a bundled Claude-backed hook that stops implementation when Codex makes a consequential choice the user's request or available specification did not settle.

## ADDED Requirements

### Requirement: Template activation is explicit and turn-scoped
The `unspecified-decisions` template SHALL be a normal Warden hook with a generated Codex marker skill. It SHALL execute only for a user turn whose submitted message contains that marker, and selection SHALL NOT carry into a later user turn unless the marker is submitted again.

#### Scenario: Marker is selected
- **WHEN** a user submits the `unspecified-decisions` marker for a Codex turn
- **THEN** the monitor receives its configured events for that turn

#### Scenario: Next turn omits the marker
- **WHEN** a later user message does not contain the marker
- **THEN** the monitor does not receive that turn's events

### Requirement: Monitor runs after implementation output
The template SHALL subscribe to successful post-tool-use, failed post-tool-use, and completed agent-message events and SHALL declare blocking execution for every subscribed event.

#### Scenario: Tool call succeeds
- **WHEN** a selected turn emits a successful post-tool-use event
- **THEN** Warden runs the monitor with the exact normalized event before releasing the native post-tool barrier

#### Scenario: Tool call fails
- **WHEN** a selected turn emits a failed post-tool-use event
- **THEN** Warden runs the monitor with the exact normalized event
- **AND** Warden does not claim it can retroactively block Codex when that observer event has no native barrier

#### Scenario: Agent response completes
- **WHEN** a selected turn emits a completed agent-message event
- **THEN** Warden runs the monitor with the exact normalized event
- **AND** it holds the native barrier when the event represents the final blockable agent response

### Requirement: Monitor retains task-specific review context
The template SHALL use one persistent Claude Sonnet conversation per source Codex task. On its first invocation for that task, the monitor SHALL obtain the current task history so it can identify the initial user request and available specification context; later selected events SHALL be sent into the same conversation.

#### Scenario: First monitored event in a task
- **WHEN** the monitor receives its first selected event for a source Codex task
- **THEN** Claude can inspect the current task history including the initial user request
- **AND** it establishes that request and available specifications as its review baseline

#### Scenario: Multiple events in one task
- **WHEN** the monitor receives later selected events from the same source Codex task
- **THEN** it resumes the same Claude conversation with the earlier review context intact

#### Scenario: Different source task
- **WHEN** the same template runs for another Codex task
- **THEN** it uses a separate Claude conversation and does not inherit the first task's context

#### Scenario: Initial review baseline is unavailable
- **WHEN** retained task history has a gap that prevents the monitor from establishing the initial request or specification baseline
- **THEN** the monitor treats the missing baseline as unsafe to continue
- **AND** it asks the user to restate or identify the governing request or specification before implementation resumes

### Requirement: Monitor identifies unspecified consequential decisions
For each subscribed event, the monitor SHALL determine whether the reported action or response commits to a product behavior, architecture, code organization, file organization, interface, dependency, operational policy, or similarly consequential choice that was not settled by the initial request or available specification. It SHALL distinguish such choices from routine implementation details that do not require user direction.

#### Scenario: Action follows specified direction
- **WHEN** the event contains no consequential decision beyond the established review baseline
- **THEN** the monitor takes no interrupt or turn-start action
- **AND** the blocking invocation completes normally

#### Scenario: Action makes an unspecified decision
- **WHEN** the event commits to a consequential choice that the review baseline does not settle
- **THEN** the monitor prepares a concise explanation of the choice and one concrete question needed from the user

### Requirement: Monitor explains and stops before work continues
When the monitor identifies an unspecified consequential decision, it SHALL interrupt the active implementation turn, wait for an observable terminal result, and then start a fresh turn in the same Codex task carrying a message that explains why work stopped and states the exact question Codex must ask the user. The template SHALL receive current-thread-history, turn-interrupt, and turn-start grants, and no broader Warden action grants.

#### Scenario: Unspecified decision is detected
- **WHEN** the monitor determines that user direction is required
- **THEN** it interrupts the active implementation turn
- **AND** only after interruption returns an observable terminal result does it start a fresh turn carrying the stop reason and one user-facing question
- **AND** the stop reason and question remain visible in the Codex task after the interrupted turn ends
- **AND** Codex does not continue implementation in that turn

#### Scenario: Interruption tears down the source turn's native bridge
- **WHEN** a successful turn-interrupt action closes the native bridge process that submitted the blocking event
- **THEN** Warden keeps the already-authenticated hook invocation alive within its configured timeout
- **AND** the monitor can execute its granted turn-start action and commit its persistent Claude session before Warden releases the invocation

#### Scenario: No decision is detected
- **WHEN** the monitor determines that user direction is not required
- **THEN** it does not interrupt the Codex turn or start a follow-up turn

#### Scenario: Agent or action execution fails
- **WHEN** Claude or a granted Warden action fails during a blocking invocation
- **THEN** Warden reports the hook failure through its normal observable error path
- **AND** it does not fabricate a successful review or a successful interruption

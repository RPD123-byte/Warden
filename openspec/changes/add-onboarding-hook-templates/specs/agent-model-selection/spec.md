## Purpose

Defines explicit model selection for agent-backed hooks without changing the provider's normal default when a hook does not request a model.

## ADDED Requirements

### Requirement: Agent hooks can request a Claude model
The fresh-inference and persistent-session Claude helpers SHALL accept an optional model name and SHALL pass a supplied non-empty model name to the Claude CLI for every process that serves that invocation or session.

#### Scenario: Fresh inference selects Sonnet
- **WHEN** a hook starts a fresh Claude inference with model `sonnet`
- **THEN** Warden launches Claude with an explicit request for the Sonnet model

#### Scenario: Persistent session selects Sonnet
- **WHEN** a hook creates or resumes a persistent Claude session with model `sonnet`
- **THEN** every Claude process serving that logical session uses the Sonnet model

#### Scenario: Model is omitted
- **WHEN** a hook invokes Claude without a model name
- **THEN** Warden leaves model selection to the Claude CLI's configured default

#### Scenario: Model name is empty
- **WHEN** a hook supplies an empty or whitespace-only model name
- **THEN** Warden rejects the invocation with a clear validation error

### Requirement: Persistent-session model is stable
A persistent agent session SHALL bind its model choice when first created. A later send to the same provider, hook, source Codex task, and logical session name SHALL NOT silently resume that conversation with a different model choice.

#### Scenario: Session resumes with the same model
- **WHEN** a later event sends to an existing persistent session using its original model choice
- **THEN** Warden resumes the same provider conversation

#### Scenario: Session model changes
- **WHEN** a later event sends to an existing persistent session with a different model choice
- **THEN** Warden rejects the send and explains that the logical session is already bound to another model

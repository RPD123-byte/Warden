## Why

Warden currently onboards the hook-authoring skill and native bridge, but a new user still starts with no working Warden hooks to inspect or invoke. Shipping repository-owned templates into a user's Warden home on first startup gives them an immediately usable example without overwriting hooks they have already created.

## What Changes

- Add canonical hook templates under the repository's `.warden/warden-hooks/` tree and embed them in the Warden CLI release.
- Reconcile missing templates into the selected global Warden home during every startup, before hook discovery and marker-skill generation, while never modifying an existing hook directory.
- Ship an `unspecified-decisions` template that is explicitly activated for one user turn through its generated marker skill.
- Run that template as a blocking hook after successful tool calls, failed tool calls, and completed agent responses.
- Use one persistent Claude Sonnet session per source Codex task to compare ongoing implementation actions with the task's initial user request and available specification context.
- Give that Claude monitor only the current-thread history, interrupt, and turn-start actions needed to identify an unspecified product or implementation decision, stop the active turn, and durably deliver one concrete user question in a fresh turn in the same task.
- Add per-invocation model selection to Claude-backed fresh and persistent agent helpers so a hook can explicitly request Sonnet.
- Document the limits of blocking observer events: Warden blocks at native Codex barriers where one exists, but cannot retroactively pause a failed tool call or intermediate agent message already emitted by Codex.

## Capabilities

### New Capabilities

- `onboarding-hook-templates`: Repository-owned Warden hook templates are safely installed into a user's selected Warden home and become discoverable through generated Codex marker skills.
- `agent-model-selection`: Agent-backed hooks can select a Claude model consistently for fresh inference and persistent sessions.
- `unspecified-decision-monitor`: A bundled, turn-scoped Claude monitor detects implementation decisions absent from the initial request/specification, interrupts the active implementation turn, and starts a fresh question-only turn to obtain user direction.

### Modified Capabilities

None.

## Impact

- Affects CLI onboarding/reconciliation, embedded assets, Python agent helpers, Rust agent request/session types, the Claude CLI driver, hook fixtures, and onboarding/runtime tests.
- Adds one repository template under `.warden/warden-hooks/`; generated marker skills remain runtime output and are not checked in as templates.
- Uses the existing Claude CLI subscription and Warden action gateway; it adds no Python package dependency and does not modify Codex's native hook bundle beyond the existing Warden bridge.

## 1. Host and Dependency Foundation

- [x] 1.1 Create the Rust host daemon workspace and add a pinned Git/Cargo dependency on the `codex-control` package from `RPD123-byte/codex-warden`.
- [x] 1.2 Add typed skill-root attachment and forced skill-refresh operations to the `codex-control` dependency, with mock app-server contract tests for initial connection and reconnection.
- [x] 1.3 Pin the compatible dependency revision in the host and add a compile-time integration test covering event streams, snapshots, retained-event queries, and `start`/`steer`/`interrupt` actions.
- [x] 1.4 Implement daemon configuration, local data roots, graceful startup/shutdown, and health reporting without duplicating `codex-control` transport supervision.

## 2. Hook Events and Turn-Scoped Activation

- [x] 2.1 Implement `HookEventKind` and `HookEvent` as a normalized view that retains the source `Arc<SequencedEvent>` and serializes normalized plus raw fields.
- [x] 2.2 Map user-message, tool-item, agent-message, and terminal-turn fixtures to the specified hook event kinds, including successful versus failed tool completion.
- [x] 2.3 Implement selected-skill extraction for structured input and Codex Desktop's marker-link representation, with canonical-path validation against the Warden generated-skill root.
- [x] 2.4 Implement activation records keyed by hook revision, thread, and turn; route only matching event kinds and expire activation on every terminal turn state.
- [x] 2.5 Add source-sequence deduplication and explicit gap handling so lifecycle replay does not execute a hook twice or fabricate a missed activation.

## 3. Hook Registry and Mandatory Marker Skills

- [x] 3.1 Implement convention-based `warden-hooks/<name>/hook.py` discovery and immutable candidate/current hook revisions without a required YAML definition.
- [x] 3.2 Generate one marker skill per valid hook with the exact body `This skill is an activation marker for the local Warden service. Ignore` and no authored hook logic.
- [x] 3.3 Attach the generated-skill root to the managed app-server and refresh skill discovery when hooks are added, changed, removed, or the app-server reconnects.
- [x] 3.4 Implement file watching and atomic revision publication so invalid candidates preserve the last valid hook and in-flight invocations retain their starting revision.
- [x] 3.5 Add tests proving a new hook becomes selectable without native `hooks.json` mutation and is inactive on the next turn unless its marker skill is selected again.

## 4. Managed Python Hook Runtime

- [x] 4.1 Create the Warden Python package with the hook decorator, normalized event model, Warden client, event enums, and importable `warden.modules` namespace.
- [x] 4.2 Implement optional `requirements.txt` resolution into isolated cached virtual environments keyed by the runtime and dependency content hash.
- [x] 4.3 Implement the supervised JSONL Python worker handshake and invocation protocol, automatically binding the incoming event as the hook function's `event` argument.
- [x] 4.4 Bound worker input, output, queueing, and execution time; isolate exceptions, crashes, malformed messages, and dependency failures by hook revision.
- [x] 4.5 Add Python integration fixtures covering a minimal dependency-free hook, a hook with a third-party dependency, hot source reload, dependency rebuild, and arbitrary logic composed with a reusable module.

## 5. Warden Action Gateway and CLI

- [x] 5.1 Define the initial typed action catalog for current event, current-thread snapshot/history, turn start, turn steer, turn interrupt, thread listing, and arbitrary-thread variants.
- [x] 5.2 Implement the daemon's local action socket, invocation credentials, action grants, and current-thread/current-turn target binding.
- [x] 5.3 Implement the `warden` CLI as a thin client of the daemon action gateway and preserve `codex-control` confirmed, rejected, and outcome-unknown results.
- [x] 5.4 Enforce selected Warden action and target scope without claiming filesystem, network, shell, or process sandboxing.
- [x] 5.5 Add authorization and contract tests for no-action hooks, current-scope hooks, deliberately cross-thread hooks, rejected commands, and ambiguous action outcomes.

## 6. Claude and Codex Agent Modules

- [x] 6.1 Define a provider-driver interface that hides subprocess startup, structured I/O, session identifiers, resume, interruption, cleanup, and failure reporting.
- [x] 6.2 Implement fresh Claude Code inference using the local user's normal subscription authentication and structured CLI streams.
- [x] 6.3 Implement fresh Codex CLI inference using non-interactive structured output.
- [x] 6.4 Implement named persistent Claude and Codex conversations keyed by provider, hook, session name, and source Codex thread, with serialized event delivery and durable resume metadata where supported.
- [x] 6.5 Make the full incoming hook event the default user message for fresh and persistent calls, while treating the hook-supplied prompt as the monitoring instruction.
- [x] 6.6 Add tests proving fresh calls share no context, persistent calls preserve ordered context, inactive Codex turns send no events, and provider crashes do not block unrelated sessions.

## 7. Hook-Creation Skill and End-to-End Verification

- [x] 7.1 Create the reusable Warden hook-creation skill with guidance for minimal Python hooks, normalized event selection, optional dependencies, optional Claude/Codex use, and defaults that omit runtime plumbing.
- [x] 7.2 Add the action multiselect to the creation workflow only when an agent-backed hook needs Warden actions, clearly distinguishing current-scope and cross-thread access.
- [x] 7.3 Add an end-to-end mock app-server test that creates a hook, generates and selects its marker skill, routes user/tool/agent events, expires the activation, and verifies no next-turn execution without reinvocation.
- [x] 7.4 Add an opt-in live macOS compatibility test for skill-root attachment, current-session skill refresh, and explicit marker activation against the installed Codex app-server version.
- [x] 7.5 Document the code-first hook examples, event enum semantics, observational `PreToolUse` limitation, dependency trust boundary, agent session choices, action grants, health diagnostics, and removal/rollback procedure.

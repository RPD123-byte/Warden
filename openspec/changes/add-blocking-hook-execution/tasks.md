## 1. Audit and dependency foundation

- [x] 1.1 Audit the existing UserPromptSubmit bridge spike against this design, preserving valid execution-mode work while removing synthetic sequence IDs, single-event assumptions, and other superseded behavior.
- [x] 1.2 Complete the typed `codex-control` APIs for listing native hook bundles and trusting one exact bundle hash, with mock app-server contract tests covering success and failure responses.
- [x] 1.3 Publish or pin a compatible `codex-app-control` revision and replace Warden's absolute local-path Cargo dependencies with reproducible dependency references.

## 2. Execution mode and normalized event model

- [x] 2.1 Complete `blocking: bool = False` metadata support across Python definitions, Rust discovery, the worker handshake, and compatibility tests for hooks that omit the field.
- [x] 2.2 Add event origin, native source metadata, and stable logical event identity to normalized events, including serialization and backward-compatibility tests.
- [x] 2.3 Update activation routing so native UserPromptSubmit events detect marker skills before dispatch and native/observed copies correlate without activating a marker on later messages.

## 3. Bounded hook dispatcher

- [x] 3.1 Replace unbounded background spawning with a supervised, bounded non-blocking queue and expose saturation and rejection diagnostics.
- [x] 3.2 Partition matching hooks by execution mode, run blocking hooks concurrently behind the current event barrier, and define ordered waiting for observer-only events without claiming to pause completed Codex work.
- [x] 3.3 Add tests for hook timeouts, cancellation, activation revocation, daemon shutdown, fail-open completion, and bounded task and child-process growth under load.

## 4. Authenticated native bridge bundle

- [x] 4.1 Implement one generic native bridge executable that validates native payloads, authenticates to Warden, uses a bounded socket protocol, and exits fail-open with neutral Codex hook output.
- [x] 4.2 Create and protect a bridge-specific credential, validate it in constant time, and ensure bridge authentication grants event submission only—not Warden action permissions.
- [x] 4.3 Implement idempotent, atomic merge and removal of Warden-owned entries in Codex's native hook configuration while preserving unrelated hooks and their ordering.
- [x] 4.4 Install fixed synchronous bridge mappings for UserPromptSubmit, PreToolUse, PostToolUse, and Stop, and translate their native payloads into the corresponding normalized Warden events.

## 5. Trust, readiness, and process ownership

- [x] 5.1 Discover the installed Warden bridge bundle through the typed app-server API and trust only its exact content hash without enabling a global trust bypass.
- [x] 5.2 Track and report bridge readiness states for configured, trusted, loaded-confirmed, and restart-required through daemon health and CLI output.
- [x] 5.3 Make managed GUI startup install bridges before the owned restart and complete exact trust before serving Warden events, while normal attach mode never quits or restarts Codex implicitly.
- [x] 5.4 Make daemon startup idempotently install the Codex-facing `create-warden-hook` authoring skill, create Warden state, reconcile bridge artifacts, attach generated marker skills, and report readiness without creating a sample hook or overwriting unrelated user files.

## 6. Event mapping and end-to-end behavior

- [x] 6.1 Test UserPromptSubmit marker activation for blocking and non-blocking hooks, including that activation applies only to the current user message and never carries into the next turn.
- [x] 6.2 Test that blocking PreToolUse hooks delay tool execution and blocking PostToolUse hooks delay Codex's continuation, while non-blocking variants release immediately.
- [x] 6.3 Test Stop mapping for final assistant responses and document/test observer-only behavior for tool failures, terminal turn events, unknown events, and assistant messages lacking an exact native payload.
- [x] 6.4 Test logical deduplication across native delivery, observed transcript delivery, retry, and replay without suppressing distinct events.
- [x] 6.5 Test mixed blocking and non-blocking hooks on one event, concurrent blocking execution, deterministic completion, and bounded saturation behavior.
- [x] 6.6 Add an opt-in live macOS integration test against the installed Codex app covering exact trust, loaded-bundle confirmation, observable pause timing, and hot updates to Warden hook definitions after bootstrap.
- [x] 6.7 Test first-run and repeated-startup onboarding against isolated Codex and Warden homes, including preservation of unrelated skills/hooks and restart-required reporting.

## 7. Authoring experience, documentation, and verification

- [x] 7.1 Update the `create-warden-hook` skill to ask for blocking versus non-blocking execution, default to non-blocking, and explain barrier-backed versus observer-only event guarantees without adding unrelated configuration fields.
- [x] 7.2 Update README and operator documentation for bridge installation, the one-time restart requirement, readiness diagnostics, marker-skill behavior, and safe bridge removal.
- [x] 7.3 Run the full Rust and Python test suites, formatting and lint checks, and strict OpenSpec validation; record any platform-specific test prerequisites.

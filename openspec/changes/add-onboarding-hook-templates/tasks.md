## 1. Claude Model Selection

- [x] 1.1 Extend the Python Claude fresh and persistent helper APIs with an optional validated `model` argument and cover serialization/default behavior in Python tests.
- [x] 1.2 Extend the authenticated agent gateway and backend contract to accept the optional model without weakening exact-event or action-grant validation.
- [x] 1.3 Carry model selection through agent requests and persist a backwards-compatible bound-model value for logical persistent sessions.
- [x] 1.4 Add Claude driver coverage proving `--model sonnet` is emitted as separate argv for fresh, new persistent, and resumed persistent calls while omission preserves provider defaults.
- [x] 1.5 Add persistence tests proving same-model resume succeeds, model changes fail clearly, and legacy model-less records remain bound to provider-default behavior.

## 2. Embedded Hook Template Onboarding

- [x] 2.1 Add the canonical `.warden/warden-hooks/unspecified-decisions/hook.py` code-first template with blocking event metadata, narrow action grants, a persistent Sonnet session, and the decision-review standing prompt.
- [x] 2.2 Add a compile-time template catalog and an atomic whole-directory installer that cleans failed staging directories and preserves any existing destination.
- [x] 2.3 Integrate template reconciliation into startup before initial hook discovery and expose installed/preserved outcomes in onboarding diagnostics.
- [x] 2.4 Add isolated-home tests for first install, repeat-start idempotence, customized and incomplete destination preservation, deleted-template restoration, custom homes, packaged embedded content, and failed installation cleanup.

## 3. Hook Runtime and codex-control Contract Tests

- [x] 3.1 Validate the installed template through the real Python registry and assert its three event kinds, blocking flag, exact action grants, and generated marker body.
- [x] 3.2 Use a fake Claude executable and action gateway to prove events resume one provider session per source Codex task, different tasks stay isolated, and the monitor obtains task history before reviewing later events.
- [x] 3.3 Add action-flow tests proving an unspecified-decision verdict steers with a reason and one question before interrupting, while a no-decision verdict performs neither action.
- [x] 3.4 Exercise Warden's codex-control seams to prove successful post-tool and final agent-response native barriers wait for the hook, and failed-tool observer events remain accurately reported as non-retroactive.
- [x] 3.5 Cover missing-baseline history gaps and provider/action failures without reporting a false successful review or interruption.

## 4. Documentation and Verification

- [x] 4.1 Update the README onboarding and hook-authoring guidance to explain bundled templates, copy-if-absent ownership, marker activation, per-turn scope, persistent per-task context, Sonnet usage, and blocking limitations.
- [x] 4.2 Run formatting, lints, the complete Rust workspace test suite, and the complete Python test suite.
- [x] 4.3 Build and install the release CLI, start it against an isolated Warden home, and verify the template and generated marker appear during that startup without changing an existing copy.
- [x] 4.4 Run an opt-in live smoke test with the local Claude subscription and a disposable Codex task to verify context reuse, steer-before-interrupt behavior, and actual blocking at supported native barriers.
- [x] 4.5 Verify the implementation against this OpenSpec change, then commit and push the completed change to `RPD123-byte/Warden` main.

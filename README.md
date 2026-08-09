# Warden

Warden adds user-authored hooks to Codex. A hook can react to prompts, tool calls, tool results,
assistant responses, and turn lifecycle events. It can run Python or call fresh or persistent Claude
or Codex sessions.

## Install

Requirements: macOS, Codex Desktop, Rust 1.91+, Python 3.11+, and an authenticated `claude` or
`codex` CLI when a hook uses that provider.

```bash
./install.sh
warden start
```

Leave `warden start` running in its own Terminal window. Startup safely onboards Codex, installs the
hook-creation skill and bundled templates, configures Warden's native bridge, and watches for hook
changes. It does not normally restart Codex.

Check the connection with:

```bash
warden health
```

`phase` should be `connected`. If `daemon.bridge.restart_required` is `true`, start a new Codex task
or restart Codex before expecting native blocking boundaries in an already-running task.

## Create a hook

Start a Codex task, select `$create-warden-hook` from the skills picker, and send:

> Walk me through creating a Warden hook.

The skill asks the necessary questions, writes the hook, checks it, and tells you how to activate
it. You can include your idea in the same message; answered questions will be skipped.

Warden stores authored hooks at `~/.warden/warden-hooks/<name>/hook.py` and automatically creates
selectable marker skills. Do not create marker skills or native Codex hooks yourself.

## Activate a hook

Select the generated `<hook-name>` skill on a message. That activates the hook only for that message
and its turn; select it again on another message to run it again.

Every hook also gets:

- `<hook-name>-start` for continuous activation in the current Codex task.
- `<hook-name>-stop` to end continuous activation.

Hooks with a persistent Claude or Codex session also get:

- `<hook-name>-pause` to suspend delivery while preserving provider context.
- `<hook-name>-resume` to continue with that context.

These are Warden controls, not Codex Goal Mode. There is no `/hooks` command.

## Included template

`unspecified-decisions` is installed on first startup. It uses a persistent Claude Sonnet session to
review tool results and completed agent responses for consequential decisions not established by
the user's request or specification. When it finds one, it interrupts the implementation and starts
a follow-up turn asking the user one question.

Its source is editable at `~/.warden/warden-hooks/unspecified-decisions/hook.py`; the repository copy
is [`.warden/warden-hooks/unspecified-decisions/hook.py`](.warden/warden-hooks/unspecified-decisions/hook.py).

## If Codex cannot attach

Stop Warden, then run this from an independent macOS Terminal:

```bash
warden start --manage-gui
```

This option may quit and relaunch Codex. Warden refuses to run it from a Codex-owned process.

## Troubleshooting

Run `warden health`, then confirm:

- the daemon is connected and its bridge is configured, trusted, and loaded;
- `~/.warden/warden-hooks/<name>/hook.py` exists;
- `~/.warden/generated-skills/<name>/SKILL.md` appeared; and
- you selected that marker on the message being tested.

Valid hook edits hot-publish without restarting Warden. An invalid edit leaves the last valid
revision active. Hook code and Python dependencies run as your local user; dependency environments
are isolated for management, not security-sandboxed.

Detailed hook APIs and manual authoring are documented in [docs/hooks.md](docs/hooks.md). Design
artifacts live under [openspec/changes](openspec/changes/).

## CLI

```text
warden start [--home PATH] [--python PATH] [--manage-gui]
warden health
warden --socket PATH health
warden action <name> --arguments '{}'
warden remove-native-bridges [--home PATH]
```

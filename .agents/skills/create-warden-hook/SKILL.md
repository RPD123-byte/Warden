---
name: create-warden-hook
description: Create or update code-first Python hooks for the local Warden daemon, including normalized Codex event selection, optional Python dependencies, optional Claude or Codex agent sessions, and explicit Warden action grants. Use when a user asks to add, change, inspect, or remove a Warden automation or wants a selectable Warden marker in the Codex prompt UI.
---

# Create Warden Hook

Create authored hook logic under `WARDEN_HOME/warden-hooks/<hook-name>/`. Treat `~/.warden` as the default `WARDEN_HOME` when the environment variable is unset.

Do not create or edit Codex native hooks. Warden startup owns the fixed native bridge bundle. Do not create the marker skill yourself. Warden generates the mandatory marker under `WARDEN_HOME/generated-skills/`, attaches that root to its managed app-server, and refreshes skill discovery.

## Gather the design

Determine:

1. The behavior and a short lowercase hook name using letters, numbers, `-`, or `_`.
2. The normalized events that should invoke it.
3. Whether it needs a third-party Python package.
4. Whether it invokes Claude or Codex, and whether each inference is fresh or persistent.
5. Whether execution is blocking or non-blocking. Default to non-blocking when the user has no preference.

Use only the event kinds needed:

- `USER_PROMPT_SUBMITTED`: normalized submitted user input.
- `TURN_STARTED`: the Codex turn began.
- `PRE_TOOL_USE`: native barrier before a tool runs when the bridge is loaded; otherwise an observed tool start.
- `POST_TOOL_USE`: native barrier after a successful tool result when the bridge is loaded.
- `POST_TOOL_USE_FAILURE`: a tool-like item completed unsuccessfully.
- `AGENT_MESSAGE_COMPLETED`: native `Stop` barrier for the final reply; earlier assistant messages remain observed.
- `TURN_COMPLETED`, `TURN_FAILED`, `TURN_INTERRUPTED`: terminal turn outcomes.
- `UNKNOWN_UPSTREAM_EVENT`: an upstream message Warden does not classify; use only for deliberate diagnostics.

`blocking=True` holds Codex only for native barrier-backed `USER_PROMPT_SUBMITTED`, `PRE_TOOL_USE`, successful `POST_TOOL_USE`, and final `AGENT_MESSAGE_COMPLETED` events. For failure, terminal, or unknown observer-only events it waits for Warden's ordered processing but cannot pause Codex work that already completed. `blocking=False` schedules the hook without holding either path.

## Select actions only for agent-backed hooks

Do not show an action multiselect for a Python-only hook.

If the hook invokes Claude or Codex, ask the user one multiselect for Warden actions. Include `None` and group the remaining options by scope:

- Current invocation: `CURRENT_EVENT`, `CURRENT_THREAD_SNAPSHOT`, `CURRENT_THREAD_HISTORY`, `TURN_START`, `TURN_STEER`, `TURN_INTERRUPT`.
- Cross-thread: `THREAD_LIST`, `ARBITRARY_THREAD_SNAPSHOT`, `ARBITRARY_THREAD_HISTORY`, `ARBITRARY_TURN_START`, `ARBITRARY_TURN_STEER`, `ARBITRARY_TURN_INTERRUPT`.

Explain in the question that current actions are bound to the triggering Codex task and turn. Cross-thread actions can inspect or control other tasks and therefore require deliberate selection. Default to `None`; never infer a cross-thread grant.

Record selected values in the `actions=[...]` argument of `@hook`. If the user explicitly asks a Python-only hook to perform a Warden action, grant only that requested action without presenting the agent-oriented multiselect.

## Author the minimal hook

Create only `hook.py` unless dependencies are required. Export exactly one decorated function accepting `event`; add the optional `warden` argument only when the function calls the host client directly.

```python
from warden import HookEvent, HookEventKind, hook


@hook(on=[HookEventKind.POST_TOOL_USE], blocking=False)
def observe_tool(event: HookEvent) -> None:
    print(f"tool item completed: {event.item_id}")
```

Use `event.payload` for normalized data and `event.raw_method` plus `event.raw_payload` when an upstream field has no normalized representation. Warden supplies `event`; do not add event-to-prompt or worker-process configuration.

For selected Warden actions, import `WardenAction` and declare the least privilege needed:

```python
from warden import HookEventKind, WardenAction, hook


@hook(
    on=[HookEventKind.AGENT_MESSAGE_COMPLETED],
    actions=[WardenAction.TURN_INTERRUPT],
)
async def inspect_reply(event, warden) -> None:
    # Call the granted action only when the hook's own logic requires it.
    await warden.interrupt_turn()
```

Add `requirements.txt` beside `hook.py` only for third-party packages. Pin versions when practical. Tell the user exactly what Warden will install and that package installation executes trusted local code under their account; it is isolated in a cached environment, not security-sandboxed.

For logic shared by several hooks, place ordinary Python modules under `WARDEN_HOME/modules/` and import them by module name. Warden snapshots that root into each published hook revision and hot-publishes module edits for later activations. Keep the hook-specific entry point in its own `hook.py`; do not copy execution logic into the generated marker skill.

## Add an agent only when requested

Use a fresh conversation by default:

```python
from warden import HookEventKind, hook
from warden.modules import claude


@hook(on=[HookEventKind.AGENT_MESSAGE_COMPLETED])
async def review_reply(event) -> None:
    await claude.run(event, prompt="Identify unstated implementation decisions.")
```

Use `codex.run` for fresh Codex inference. The full event is automatically the provider's user message; `prompt` is the monitoring instruction.

Use an explicit named session only when later active invocations need earlier conversation context:

```python
from warden import HookEventKind, hook
from warden.modules import claude


monitor = claude.session(
    "decision-monitor",
    prompt="Track implementation decisions across selected events.",
)


@hook(on=[HookEventKind.USER_PROMPT_SUBMITTED, HookEventKind.AGENT_MESSAGE_COMPLETED])
async def monitor_decisions(event) -> None:
    await monitor.send(event)
```

Use `codex.session` for persistent Codex context. A persistent session receives nothing from turns where the marker was not selected.

## Verify and hand off

1. Check Python syntax, require a JSON-serializable return value when the hook returns anything, and inspect the final files without exposing secrets.
2. Let Warden's file watcher validate and publish the candidate; do not invent a manual reload command.
3. Run `warden health` and inspect daemon logs if publication fails.
4. Confirm `WARDEN_HOME/generated-skills/<hook-name>/SKILL.md` appears. Its instruction body must be exactly `This skill is an activation marker for the local Warden service. Ignore`.
5. Tell the user to select that generated skill on every Codex message that should activate the hook. Selection applies only to that turn.

When updating a hook, preserve unrelated user logic. An invalid candidate must leave the last valid revision active. To remove a hook recoverably, move its authored directory outside `warden-hooks/`; Warden removes the generated marker and stops new activations.

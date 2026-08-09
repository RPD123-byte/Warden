---
name: create-warden-hook
description: Walk the user through designing, creating, updating, or removing a Warden hook for Codex. Use when the user invokes $create-warden-hook, asks to be guided through Warden hook creation, or requests a Warden automation with Python logic, Claude or Codex inference, event filters, blocking behavior, dependencies, or Codex control actions.
---

# Create Warden Hook

Guide the user from an idea to a working hook. Treat `~/.warden` as `WARDEN_HOME` when the environment variable is unset.

Do not create Codex native hooks or generated marker skills. Warden owns both. Author only under `WARDEN_HOME/warden-hooks/<hook-name>/`; Warden validates the hook, publishes it, and generates its selectable marker.

## Run the walkthrough

Ask only about choices the user has not already made. Ask one short group of questions at a time, using structured choice or multiselect UI when available. Explain choices in plain language. Do not make the user know Warden terminology.

1. Establish what the hook should notice and what it should do. Derive a short lowercase name containing only letters, numbers, `-`, or `_`, and let the user correct it.
2. Ask which events should run it:
   - submitted user prompt (`USER_PROMPT_SUBMITTED`)
   - turn started (`TURN_STARTED`)
   - before a tool runs (`PRE_TOOL_USE`)
   - after a successful tool result (`POST_TOOL_USE`)
   - after a failed tool result (`POST_TOOL_USE_FAILURE`)
   - completed assistant response (`AGENT_MESSAGE_COMPLETED`)
   - completed, failed, or interrupted turn (`TURN_COMPLETED`, `TURN_FAILED`, `TURN_INTERRUPTED`)
3. Ask how it should run:
   - ordinary Python only;
   - fresh Claude or Codex inference on every matching event; or
   - one persistent Claude or Codex conversation per source Codex task.
4. Ask whether Codex must wait for it. Default to non-blocking when the user has no preference. Explain that blocking truly pauses Codex only at native barriers: submitted prompts, before tools, successful tool results, and the final assistant response. Other events can be ordered but cannot pause work that already finished.
5. Ask whether it needs third-party Python packages. Do not add packages speculatively.
6. For an agent-backed hook, ask one multiselect for allowed Warden actions. Default to `None`; never infer cross-task access:
   - Current task: `CURRENT_EVENT`, `CURRENT_THREAD_SNAPSHOT`, `CURRENT_THREAD_HISTORY`, `TURN_START`, `TURN_STEER`, `TURN_INTERRUPT`.
   - Other tasks: `THREAD_LIST`, `ARBITRARY_THREAD_SNAPSHOT`, `ARBITRARY_THREAD_HISTORY`, `ARBITRARY_TURN_START`, `ARBITRARY_TURN_STEER`, `ARBITRARY_TURN_INTERRUPT`.

Explain that current-task actions are bound to the triggering task and turn, while cross-task actions can inspect or control other Codex tasks. Skip this question for Python-only hooks unless the requested behavior directly requires a Warden action; then grant only that action.

Once the choices are clear, briefly recap them and implement the hook. Do not require another confirmation unless the recap exposes a consequential ambiguity.

## Author the hook

Create only `hook.py` unless dependencies are required. Export exactly one decorated function accepting `event`; add `warden` only when calling a granted host action.

```python
from warden import HookEvent, HookEventKind, hook


@hook(on=[HookEventKind.POST_TOOL_USE], blocking=False)
def observe_tool(event: HookEvent) -> None:
    print(f"tool item completed: {event.item_id}")
```

Use `event.payload` for normalized data. Use `event.raw_method` and `event.raw_payload` only when the normalized event omits a required upstream field. Warden supplies the in-memory `event`; do not configure event-to-prompt transforms or worker lifetimes.

Declare granted actions on `@hook` with `WardenAction`. Use the least privilege selected by the user.

For fresh agent inference:

```python
from warden import HookEventKind, hook
from warden.modules import claude


@hook(on=[HookEventKind.AGENT_MESSAGE_COMPLETED])
async def review_reply(event) -> None:
    await claude.run(event, prompt="Identify unsupported claims.")
```

Use `codex.run` for fresh Codex inference. The full event is automatically sent as the provider's user message.

For persistent context, declare the named session at module scope:

```python
from warden import HookEventKind, hook
from warden.modules import claude


monitor = claude.session("decision-monitor", prompt="Track implementation decisions.")


@hook(on=[HookEventKind.USER_PROMPT_SUBMITTED, HookEventKind.AGENT_MESSAGE_COMPLETED])
async def monitor_decisions(event) -> None:
    await monitor.send(event)
```

Use `codex.session` for persistent Codex context. A persistent conversation is isolated per source Codex task and receives nothing from turns where the hook is inactive.

Place `requirements.txt` beside `hook.py` only for third-party packages. Pin versions when practical. Tell the user that Warden installs them into a cached environment, but hook code and packages still run as the local user and are not security-sandboxed.

Put shared Python modules under `WARDEN_HOME/modules/`. Keep hook-specific execution in its own `hook.py`.

## Verify and hand off

1. Check Python syntax and require JSON-serializable return values.
2. Let Warden's watcher validate and publish the change; there is no reload command.
3. Run `warden health` and inspect daemon output if publication fails.
4. Confirm `WARDEN_HOME/generated-skills/<hook-name>/SKILL.md` appears. Never repair it manually.
5. Explain how to activate the generated marker:
   - selecting `<hook-name>` activates it for one message and that turn;
   - every hook also gets `<hook-name>-start` and `<hook-name>-stop`;
   - hooks with a module-scope persistent agent session also get `<hook-name>-pause` and `<hook-name>-resume`.

When updating a hook, preserve unrelated user logic. An invalid candidate leaves the last valid revision active. For recoverable removal, move the authored hook directory outside `warden-hooks/`; Warden removes its generated markers and stops new activations.

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import textwrap


SRC = Path(__file__).parents[1] / "src"


def event(kind="post_tool_use"):
    return {
        "sequence": 91,
        "kind": kind,
        "thread_id": "thread-worker",
        "turn_id": "turn-worker",
        "item_id": "item-worker",
        "payload": {"output": "ok"},
        "raw_method": "item/completed",
        "raw_payload": {"params": {"item": "raw"}},
    }


def run_worker(tmp_path: Path, source: str, messages, *, environment_overrides=None):
    hook_dir = tmp_path / "sample-hook"
    hook_dir.mkdir()
    hook_file = hook_dir / "hook.py"
    hook_file.write_text(textwrap.dedent(source))
    environment = os.environ.copy()
    environment["PYTHONPATH"] = str(SRC)
    environment.update(environment_overrides or {})
    process = subprocess.run(
        [
            sys.executable,
            "-m",
            "warden.worker",
            "--hook",
            str(hook_file),
            "--hook-name",
            "sample-hook",
        ],
        input="".join(json.dumps(message) + "\n" for message in messages),
        text=True,
        capture_output=True,
        env=environment,
        timeout=5,
        check=False,
    )
    return process, [json.loads(line) for line in process.stdout.splitlines()]


def test_worker_handshakes_then_invokes_async_hook_with_bound_event_and_client(
    tmp_path,
):
    process, output = run_worker(
        tmp_path,
        """
        from warden import hook, HookEventKind, WardenAction
        print("loaded hook")

        @hook(
            on=[HookEventKind.POST_TOOL_USE],
            actions=[WardenAction.CURRENT_EVENT, WardenAction.TURN_INTERRUPT],
        )
        async def handle(event, warden):
            print(f"handled {event.sequence}")
            return {
                "kind": event.kind.value,
                "invocation_id": warden.invocation_id,
                "token": warden.token,
            }
        """,
        [
            {
                "type": "invoke",
                "protocol_version": 1,
                "id": "request-1",
                "event": event(),
                "warden": {"invocation_id": "inv-91", "token": "secret"},
            }
        ],
    )

    assert process.returncode == 0, process.stderr
    handshake, result = output
    assert handshake["type"] == "handshake"
    assert handshake["ok"] is True
    assert handshake["hook"] == {
        "name": "sample-hook",
        "function": "handle",
        "events": ["post_tool_use"],
        "actions": ["current_event", "turn_interrupt"],
        "blocking": False,
        "is_async": True,
    }
    assert handshake["logs"]["stdout"] == "loaded hook\n"
    assert result == {
        "type": "result",
        "id": "request-1",
        "ok": True,
        "result": {
            "kind": "post_tool_use",
            "invocation_id": "inv-91",
            "token": "secret",
        },
        "logs": {"stdout": "handled 91\n", "stderr": ""},
    }


def test_worker_invokes_sync_hook_and_isolates_exception(tmp_path):
    process, output = run_worker(
        tmp_path,
        """
        from warden import hook, HookEventKind

        @hook(on=HookEventKind.POST_TOOL_USE)
        def handle(event, warden):
            print("before failure")
            raise RuntimeError("hook exploded")
        """,
        [{"type": "invoke", "id": "request-2", "event": event()}],
    )

    assert process.returncode == 0
    result = output[1]
    assert result["ok"] is False
    assert result["id"] == "request-2"
    assert result["error"]["code"] == "hook_exception"
    assert result["error"]["exception_type"] == "RuntimeError"
    assert result["error"]["message"] == "hook exploded"
    assert "RuntimeError: hook exploded" in result["error"]["traceback"]
    assert result["logs"]["stdout"] == "before failure\n"


def test_worker_invokes_minimal_event_only_hook(tmp_path):
    process, output = run_worker(
        tmp_path,
        """
        from warden import hook, HookEventKind

        @hook(on=HookEventKind.POST_TOOL_USE)
        def run(event):
            return {"sequence": event.sequence}
        """,
        [{"type": "invoke", "id": "request-minimal", "event": event()}],
    )

    assert process.returncode == 0
    assert output[0]["hook"]["actions"] == []
    assert output[1]["ok"] is True
    assert output[1]["result"] == {"sequence": 91}


def test_worker_rejects_event_not_declared_by_hook(tmp_path):
    process, output = run_worker(
        tmp_path,
        """
        from warden import hook, HookEventKind

        @hook(on=HookEventKind.TURN_COMPLETED)
        def handle(event, warden):
            raise AssertionError("must not run")
        """,
        [{"type": "invoke", "id": "request-3", "event": event()}],
    )

    assert process.returncode == 0
    assert output[1]["ok"] is False
    assert output[1]["error"]["code"] == "event_not_registered"


def test_worker_reports_load_failure_as_failed_handshake(tmp_path):
    process, output = run_worker(
        tmp_path,
        """
        def handle(event, warden):
            return None
        """,
        [],
    )

    assert process.returncode == 1
    assert output[0]["type"] == "handshake"
    assert output[0]["ok"] is False
    assert output[0]["error"]["code"] == "hook_load_failed"


def test_worker_bounds_import_and_invocation_logs_while_reporting_omissions(tmp_path):
    process, output = run_worker(
        tmp_path,
        """
        from warden import hook, HookEventKind
        print("i" * 200, end="")

        @hook(on=HookEventKind.POST_TOOL_USE)
        def handle(event):
            print("h" * 300, end="")
        """,
        [{"type": "invoke", "id": "bounded-logs", "event": event()}],
        environment_overrides={"WARDEN_MAX_LOG_CHARS": "32"},
    )

    assert process.returncode == 0, process.stderr
    assert output[0]["logs"]["stdout"] == ("i" * 32 + "\n...[truncated 168 characters]")
    assert output[1]["logs"]["stdout"] == ("h" * 32 + "\n...[truncated 268 characters]")


def test_bounded_capture_never_retains_more_than_its_limit():
    from warden.worker import BoundedTextCapture

    capture = BoundedTextCapture(8)
    assert capture.write("abcdefghij") == 10
    assert capture.write("klm") == 3

    assert capture.stored_characters == 8
    assert capture.omitted_characters == 5
    assert capture.render() == "abcdefgh\n...[truncated 5 characters]"

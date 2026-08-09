from __future__ import annotations

import asyncio

from warden.client import bind_client
from warden.modules import tools


class FakeWarden:
    def __init__(self):
        self.calls = []

    async def interrupt_turn(self):
        self.calls.append(("interrupt_turn",))
        return {"status": "interrupted"}

    async def current_thread_history(self, *, after=None, through=None):
        self.calls.append(("current_thread_history", after, through))
        return ["event"]

    async def arbitrary_turn_steer(self, thread_id, turn_id, input):
        self.calls.append(("arbitrary_turn_steer", thread_id, turn_id, input))
        return {"status": "steered"}


def test_tools_use_explicit_or_worker_bound_invocation_client():
    async def scenario():
        explicit = FakeWarden()
        assert await tools.interrupt_turn(warden=explicit) == {"status": "interrupted"}

        bound = FakeWarden()
        with bind_client(bound):
            assert await tools.current_thread_history(after=4, through=9) == ["event"]
            assert await tools.arbitrary_turn_steer(
                "thread", "turn", [{"type": "text", "text": "continue"}]
            ) == {"status": "steered"}
        return explicit, bound

    explicit, bound = asyncio.run(scenario())
    assert explicit.calls == [("interrupt_turn",)]
    assert bound.calls == [
        ("current_thread_history", 4, 9),
        (
            "arbitrary_turn_steer",
            "thread",
            "turn",
            [{"type": "text", "text": "continue"}],
        ),
    ]

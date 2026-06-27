from __future__ import annotations

import asyncio
import datetime as dt
import importlib.util
import sys
import unittest
from pathlib import Path
from unittest.mock import patch


def load_workflow_host():
    module_path = Path(__file__).resolve().parents[1] / "workflow_host.py"
    spec = importlib.util.spec_from_file_location("workflow_host_under_test", module_path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class FakePool:
    def __init__(self) -> None:
        self.closed = False

    async def close(self) -> None:
        self.closed = True


class FakeRpc:
    def __init__(self) -> None:
        self.drained = False

    async def drain_notifications(self) -> None:
        self.drained = True


class RecordingRpc:
    def __init__(self) -> None:
        self.requests: list[dict] = []

    async def request(self, payload: dict):
        self.requests.append(payload)
        return {"ok": True}

    def notify(self, payload: dict) -> None:
        del payload


class WorkflowHostTests(unittest.TestCase):
    def test_workflow_result_includes_grouping_identifiers(self) -> None:
        host = load_workflow_host()
        pool = FakePool()
        rpc = FakeRpc()

        async def handler(inp, ctx):
            self.assertEqual(inp, {"input": "value"})
            return {"ok": True, "seen_run_id": ctx.run_id}

        registered = host.RegisteredWorkflow(
            workflow_name="sample_workflow",
            source_path="workflows/sample.py",
            handler=handler,
            input_cls=None,
            webhooks=None,
            schedule=None,
        )

        async def create_pool():
            return pool

        with (
            patch.object(
                host,
                "discover_workflows",
                return_value={"sample_workflow": registered},
            ),
            patch.object(host, "create_pool", create_pool),
        ):
            payload = asyncio.run(
                host.run_workflow(
                    {
                        "type": "workflow.start",
                        "workflow_name": "sample_workflow",
                        "run_id": "run-123",
                        "task_id": "task-456",
                        "input": {"input": "value"},
                    },
                    rpc,
                )
            )

        self.assertEqual(
            payload,
            {
                "type": "workflow.result",
                "workflow_run_id": "run-123",
                "run_id": "run-123",
                "workflow_task_id": "task-456",
                "task_id": "task-456",
                "workflow_name": "sample_workflow",
                "result": {"ok": True, "seen_run_id": "run-123"},
            },
        )
        self.assertTrue(rpc.drained)
        self.assertTrue(pool.closed)

    def test_sleep_helpers_send_context_rpc_requests(self) -> None:
        host = load_workflow_host()
        rpc = RecordingRpc()
        ctx = host.WorkflowContext(
            rpc,
            run_id="run-123",
            task_id="task-456",
            workflow_name="sample_workflow",
        )

        asyncio.run(
            ctx.sleep_until(
                "wake",
                dt.datetime(2026, 6, 27, 12, 30, tzinfo=dt.timezone.utc),
            )
        )
        asyncio.run(ctx.sleep_for("settle", dt.timedelta(seconds=2.5)))

        self.assertEqual(
            rpc.requests,
            [
                {
                    "type": "ctx.sleep_until",
                    "step": "wake",
                    "wake_at": "2026-06-27T12:30:00+00:00",
                },
                {
                    "type": "ctx.sleep_for",
                    "step": "settle",
                    "seconds": 2.5,
                },
            ],
        )


if __name__ == "__main__":
    unittest.main()

from __future__ import annotations

import asyncio
import importlib.util
import json
import os
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
WORKFLOW = ROOT / "workflows" / "approval_hello_world.py"
HOST = ROOT / "services" / "workflow-python" / "workflow_host.py"


def load_workflow():
    spec = importlib.util.spec_from_file_location("approval_hello_world", WORKFLOW)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class NoSideEffects:
    def __getattr__(self, name):
        raise AssertionError(f"Hello-world must not invoke context operation {name}")


class ApprovalHelloWorldTests(unittest.TestCase):
    def test_declares_approval_only_principal_without_other_entry_points(self):
        workflow = load_workflow()
        self.assertEqual(workflow.WORKFLOW_NAME, "approval_hello_world")
        self.assertIs(workflow.WORKFLOW_PRINCIPAL, True)
        self.assertIs(workflow.WORKFLOW_REQUIRES_APPROVAL, True)
        self.assertFalse(hasattr(workflow, "SCHEDULE"))
        self.assertFalse(hasattr(workflow, "WEBHOOKS"))

    def test_returns_fixed_greeting_without_credentials_or_side_effects(self):
        with patch.dict(
            os.environ,
            {"CENTAUR_APPROVAL_PROTOCOL": "workflow-tool-approvals-v1"},
            clear=True,
        ):
            result = asyncio.run(load_workflow().handler({}, NoSideEffects()))
        self.assertEqual(result, {"message": "Hello world!"})

    def test_rejects_ordinary_or_older_host_and_forged_input_proof(self):
        for protocol in ["", "tool-approvals-v1", "caller-supplied"]:
            with (
                self.subTest(protocol=protocol),
                patch.dict(
                    os.environ, {"CENTAUR_APPROVAL_PROTOCOL": protocol}, clear=True
                ),
            ):
                for inp in [{}, {"approval": True, "click": {"user_id": "UAPPROVER"}}]:
                    with self.assertRaises(RuntimeError):
                        asyncio.run(load_workflow().handler(inp, NoSideEffects()))

    def test_does_not_echo_input_or_accept_alternate_operations(self):
        with patch.dict(
            os.environ,
            {"CENTAUR_APPROVAL_PROTOCOL": "workflow-tool-approvals-v1"},
            clear=True,
        ):
            for inp in [
                None,
                [],
                "hello",
                {"message": "unreviewed"},
                {"url": "https://example.invalid"},
            ]:
                with self.subTest(inp=inp), self.assertRaises(ValueError):
                    asyncio.run(load_workflow().handler(inp, NoSideEffects()))

    def test_real_host_discovers_and_runs_the_checked_in_executor(self):
        # This proves the real NDJSON host boundary, not Slack authorization or
        # sandbox isolation. Core is responsible for injecting the marker.
        messages = [
            {"type": "workflow.discover"},
            {
                "type": "workflow.start",
                "run_id": "test-run",
                "task_id": "test-task",
                "workflow_name": "approval_hello_world",
                "input": {},
            },
        ]
        env = {
            **os.environ,
            "DATABASE_URL": "",
            "WORKFLOW_DIRS": str(ROOT / "workflows"),
            "WORKFLOW_ENABLE_MODE": "allowlist",
            "WORKFLOW_ALLOWED_NAMES": "approval_hello_world",
            "CENTAUR_APPROVAL_PROTOCOL": "workflow-tool-approvals-v1",
        }
        output = []
        # Discovery and execution use separate short-lived host processes.
        for message in messages:
            result = subprocess.run(
                [sys.executable, str(HOST)],
                input=json.dumps(message) + "\n",
                text=True,
                capture_output=True,
                timeout=15,
                env=env,
                check=True,
            )
            output.extend(json.loads(line) for line in result.stdout.splitlines())
        discovery = next(
            message for message in output if message["type"] == "workflow.discovery"
        )
        self.assertEqual(len(discovery["workflows"]), 1)
        self.assertTrue(discovery["workflows"][0]["requires_approval"])
        self.assertEqual(
            discovery["workflows"][0]["workflow_name"], "approval_hello_world"
        )
        completed = next(
            message for message in output if message["type"] == "workflow.result"
        )
        self.assertEqual(completed["result"], {"message": "Hello world!"})
        self.assertFalse(any(message["type"].startswith("ctx.") for message in output))


if __name__ == "__main__":
    unittest.main()

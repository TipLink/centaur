"""The Slack RPC forwards operation data without injecting service credentials."""
import asyncio
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from api.workflow_engine import WorkflowContext


class SlackTransportTests(unittest.TestCase):
    def test_slack_transport_rpc_contract(self):
        calls = []

        class Rpc:
            async def request(self, payload):
                calls.append(payload)
                return {'ok': True, 'ts': '123.456'}

        context = WorkflowContext(Rpc(), run_id='run-1', task_id='task-1', workflow_name='work_item_job')
        args = {'team_id': 'T1', 'channel': 'C1', 'thread_ts': '123.000', 'text': 'Reminder'}
        self.assertEqual(asyncio.run(context.slack_transport('post', args)), {'ok': True, 'ts': '123.456'})
        self.assertEqual(calls, [{'type': 'ctx.slack_transport', 'operation': 'post', 'args': args}])

    def test_slack_transport_propagates_delivery_failure(self):
        class Rpc:
            async def request(self, payload):
                raise RuntimeError('transport unavailable')

        context = WorkflowContext(Rpc(), run_id='run-1', task_id='task-1', workflow_name='work_item_job')
        with self.assertRaisesRegex(RuntimeError, 'transport unavailable'):
            asyncio.run(context.slack_transport('post', {'channel': 'C1', 'text': 'Reminder'}))

import importlib.util
import json
import unittest
from pathlib import Path

import httpx

spec = importlib.util.spec_from_file_location("approvals", Path(__file__).with_name("client.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class ApprovalClientTest(unittest.TestCase):
    def test_request_uses_current_execution_and_same_key_without_actor_fields(self):
        calls = []
        key = "00000000-0000-4000-8000-000000000001"

        def handle(request):
            calls.append(request)
            if request.url.path.endswith("/context"):
                return httpx.Response(
                    200, json={"data": {"execution_id": "exe_current", "actions": []}}
                )
            body = json.loads(request.content)["data"]
            self.assertEqual(
                body,
                {
                    "execution_id": "exe_current",
                    "idempotency_key": key,
                    "action": "create",
                    "arguments": {"body": "hello"},
                },
            )
            self.assertNotIn("authorization", request.headers)
            return httpx.Response(202, json={"data": {"id": key}})

        client = module.ApprovalsClient(transport=httpx.MockTransport(handle))
        self.assertEqual(client.request("create", {"body": "hello"}, key), {"id": key})
        self.assertEqual(len(calls), 2)
        client.close()

    def test_decline_and_uncertain_outcome_never_resubmit(self):
        for status in ["declined", "unknown", "cancelled", "expired"]:
            calls = []

            def handle(request, calls=calls, status=status):
                calls.append(request.method)
                return httpx.Response(200, json={"data": {"status": status}})

            client = module.ApprovalsClient(transport=httpx.MockTransport(handle))
            self.assertEqual(client.wait("00000000-0000-4000-8000-000000000001")["status"], status)
            self.assertEqual(calls, ["GET"])
            client.close()

import importlib.util
import json
import unittest
from pathlib import Path
from unittest.mock import patch

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

    def test_cancel_is_a_scoped_request_not_a_resubmission(self):
        key = "00000000-0000-4000-8000-000000000001"

        def handle(request):
            self.assertEqual(request.method, "POST")
            self.assertEqual(request.url.path, module.PATH + "/" + key + "/cancel")
            self.assertNotIn("authorization", request.headers)
            return httpx.Response(200, json={"data": {"status": "cancelled"}})

        client = module.ApprovalsClient(transport=httpx.MockTransport(handle))
        self.assertEqual(client.cancel(key)["status"], "cancelled")
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

    def test_wait_returns_published_hello_world_to_the_requesting_bot(self):
        calls = []
        completed = {
            "status": "succeeded",
            "result": {"outcome": "succeeded", "output": {"message": "Hello world!"}},
        }
        responses = [{"status": "pending"}, {"status": "approved"}, completed]

        def handle(request):
            calls.append(request.method)
            return httpx.Response(200, json={"data": responses.pop(0)})

        client = module.ApprovalsClient(transport=httpx.MockTransport(handle))
        with patch.object(module.time, "sleep"):
            self.assertEqual(client.wait("00000000-0000-4000-8000-000000000001"), completed)
        self.assertEqual(calls, ["GET", "GET", "GET"])
        client.close()

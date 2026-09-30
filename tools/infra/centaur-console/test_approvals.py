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
    def test_errors_and_redirects_never_expose_response_bodies(self):
        for response in [
            httpx.Response(503, text="private-provider-output"),
            httpx.Response(302, headers={"location": "https://untrusted.invalid/private"}),
            httpx.Response(200, text="private-invalid-json"),
            httpx.Response(200, json=["private-invalid-shape"]),
        ]:
            calls = []

            def handle(request, calls=calls, response=response):
                calls.append(request)
                return response

            with module.ConsoleClient(transport=httpx.MockTransport(handle)) as client:
                with self.assertRaises(RuntimeError) as error:
                    client.approval_actions()
                self.assertNotIn("private", str(error.exception))
                self.assertEqual(len(calls), 1)

    def test_transport_errors_are_sanitized(self):
        def handle(request):
            raise httpx.ConnectError("private-provider-output", request=request)

        with module.ConsoleClient(transport=httpx.MockTransport(handle)) as client:
            with self.assertRaisesRegex(RuntimeError, "Approval API request unavailable") as error:
                client.approval_actions()
            self.assertIsNone(error.exception.__cause__)
            self.assertTrue(error.exception.__suppress_context__)

    def test_timeout_does_not_cancel_or_resubmit(self):
        calls = []

        def handle(request):
            calls.append(request.method)
            return httpx.Response(200, json={"data": {"status": "pending"}})

        with (
            module.ConsoleClient(transport=httpx.MockTransport(handle)) as client,
            self.assertRaisesRegex(TimeoutError, "do not resubmit"),
        ):
            client.wait_for_approval("00000000-0000-4000-8000-000000000001", timeout_seconds=0)
        self.assertEqual(calls, ["GET"])

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

        client = module.ConsoleClient(transport=httpx.MockTransport(handle))
        self.assertEqual(client.request_approval("create", {"body": "hello"}, key), {"id": key})
        self.assertEqual(len(calls), 2)
        client.close()

    def test_cancel_is_a_scoped_request_not_a_resubmission(self):
        key = "00000000-0000-4000-8000-000000000001"

        def handle(request):
            self.assertEqual(request.method, "POST")
            self.assertEqual(
                request.url.path, module.SANDBOX_APPROVALS_PATH + "/" + key + "/cancel"
            )
            self.assertNotIn("authorization", request.headers)
            return httpx.Response(200, json={"data": {"status": "cancelled"}})

        client = module.ConsoleClient(transport=httpx.MockTransport(handle))
        self.assertEqual(client.cancel_approval(key)["status"], "cancelled")
        client.close()

    def test_decline_and_uncertain_outcome_never_resubmit(self):
        for status in ["declined", "unknown", "cancelled", "expired"]:
            calls = []

            def handle(request, calls=calls, status=status):
                calls.append(request.method)
                return httpx.Response(200, json={"data": {"status": status}})

            client = module.ConsoleClient(transport=httpx.MockTransport(handle))
            self.assertEqual(
                client.wait_for_approval("00000000-0000-4000-8000-000000000001")["status"], status
            )
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

        client = module.ConsoleClient(transport=httpx.MockTransport(handle))
        with patch.object(module.time, "sleep"):
            self.assertEqual(
                client.wait_for_approval("00000000-0000-4000-8000-000000000001"), completed
            )
        self.assertEqual(calls, ["GET", "GET", "GET"])
        client.close()

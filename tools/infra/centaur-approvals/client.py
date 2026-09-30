"""Sandbox-scoped approval client. Provider credentials never enter this process."""

from __future__ import annotations

import os
import time
from typing import Any
from uuid import UUID, uuid4

import httpx

PATH = "/api/v1/sandbox/tool_approvals"
TERMINAL = frozenset({"succeeded", "failed", "unknown", "declined", "expired", "cancelled"})


class ApprovalsClient:
    def __init__(self, url: str | None = None, transport: httpx.BaseTransport | None = None):
        self._http = httpx.Client(
            # Non-secret endpoint configuration, as in centaur-console.
            base_url=url or os.getenv("CENTAUR_CONSOLE_URL", "http://centaur-console:3000"),  # noqa: TID251
            timeout=10,
            transport=transport,
            follow_redirects=False,
        )

    def _request(self, method: str, path: str, body: dict | None = None) -> dict[str, Any]:
        # The existing proxy supplies a sandbox entitlement at this exact Console
        # origin. Do not take an actor, principal, destination, or token from args.
        response = self._http.request(method, path, json=body)
        if not response.is_success:
            raise RuntimeError(f"Approval API returned HTTP {response.status_code}")
        data = response.json().get("data")
        if not isinstance(data, dict):
            raise RuntimeError("Invalid approval response")
        return data

    def actions(self) -> dict[str, Any]:
        """List actions permitted for the currently running Slack execution."""
        return self._request("GET", PATH + "/context")

    def request(
        self, action: str, arguments: dict[str, Any], idempotency_key: str | None = None
    ) -> dict[str, Any]:
        """Freeze arguments and request a single approval; never execute locally."""
        if not isinstance(arguments, dict):
            raise ValueError("arguments must be a JSON object")
        context = self.actions()
        key = str(UUID(idempotency_key)) if idempotency_key else str(uuid4())
        return self._request(
            "POST",
            PATH,
            {
                "data": {
                    "execution_id": context["execution_id"],
                    "idempotency_key": key,
                    "action": action,
                    "arguments": arguments,
                }
            },
        )

    def status(self, request_id: str) -> dict[str, Any]:
        """Read one request owned by this sandbox; does not resubmit it."""
        return self._request("GET", PATH + "/" + str(UUID(request_id)))

    def wait(self, request_id: str, timeout_seconds: int = 1200) -> dict[str, Any]:
        """Wait for a decision and result. A client timeout does not cancel the request."""
        deadline = time.monotonic() + timeout_seconds
        while True:
            result = self.status(request_id)
            if result["status"] in TERMINAL:
                return result
            if time.monotonic() >= deadline:
                raise TimeoutError(f"Still pending: {request_id}. Use status; do not resubmit.")
            time.sleep(2)

    def cancel(self, request_id: str) -> dict[str, Any]:
        """Cancel pending/approved work; cannot undo execution already claimed."""
        return self._request("POST", PATH + "/" + str(UUID(request_id)) + "/cancel")

    def close(self) -> None:
        self._http.close()


def _client() -> ApprovalsClient:
    return ApprovalsClient()

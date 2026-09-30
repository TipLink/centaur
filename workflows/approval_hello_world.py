"""Credential-free, approval-only executor for testing the Slack round trip."""

from __future__ import annotations

import os
from typing import Any

WORKFLOW_NAME = "approval_hello_world"
WORKFLOW_PRINCIPAL = True
WORKFLOW_REQUIRES_APPROVAL = True


async def handler(inp: Any, ctx: Any) -> dict[str, str]:
    # Defense against older hosts that ignore WORKFLOW_REQUIRES_APPROVAL.
    # Core admission and its one-shot claim, not an input flag, authorize us.
    if os.environ.get("CENTAUR_APPROVAL_PROTOCOL") != "workflow-tool-approvals-v1":
        raise RuntimeError("A workflow-native approval runtime is required")
    if not isinstance(inp, dict) or inp:
        raise ValueError(
            "The hello-world executor accepts only an empty argument object"
        )
    # No tools, network, credentials, agent turns, or other side effects.
    return {"message": "Hello world!"}

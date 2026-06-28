---
title: Creating Workflows
description: Add durable Centaur workflows with checkpointed steps, sleeps, schedules, webhooks, Slack posts, tool calls, and agent turns.
---

# Creating Workflows

Workflows are Python handlers that run through Centaur's durable api-rs
workflow engine. They are useful when the task is longer than one agent turn:
polling, branching, retries, scheduled syncs, webhook handling, or coordinating
agent runs.

Use a workflow when the system needs durable progress rather than a single
request-response turn. Common examples include scheduled reports, ETL syncs,
incident monitors, approval gates, webhook-driven triage, long-running research
jobs, and multi-agent handoffs that need to survive deploys or sandbox restarts.

Put organization workflows in an overlay repo under `workflows/`. See
[Using an overlay](/extend/overlay) for packaging, mount paths, and chart
configuration.

Migrating existing workflows to the api-rs Absurd runtime? See
[Workflows v2 Migration](/extend/workflows-v2).

Workflows are loaded from `WORKFLOW_DIRS`. In an overlay deployment, workflow
files must exist under the source's `workflowsSubdir` — by default
`workflows/` — in its repo-cache checkout, for example
`/var/lib/centaur/repos/your-org/centaur-overlay/workflows` in the API
container. Workflow-host sandboxes receive the same ordered list translated to
`/home/agent/github/...`. Files in those directories are loaded the same way as
built-in workflows; sources without the directory are skipped.

## Define a workflow

Each workflow file exports `WORKFLOW_NAME` and an async `handler(params, ctx)`.
An optional `Input` dataclass gives structured inputs.

```python
from dataclasses import dataclass
from typing import Any

from api.workflow_engine import WorkflowContext


WORKFLOW_NAME = "nightly_report"


@dataclass
class Input:
    channel: str
    topic: str


async def handler(inp: Input, ctx: WorkflowContext) -> dict[str, Any]:
    data = await ctx.step("collect", lambda: {"topic": inp.topic})
    await ctx.sleep_for("settle", 30)
    result = await ctx.run_agent(
        f"Write a short report about {data['topic']}",
        thread_key=f"workflow:{ctx.run_id}:nightly_report",
    )
    return {"channel": inp.channel, "report": result}
```

## Durable primitives

| Primitive | Use it for |
|-----------|------------|
| `ctx.step(name, fn)` | Run a deterministic or idempotent operation and cache its result after the callback succeeds. |
| `ctx.sleep_for(name, seconds)` | Suspend and resume later. |
| `ctx.sleep_until(name, when)` | Resume at a specific time. |
| `ctx.agent_turn(text, **kwargs)` / `ctx.run_agent(text, **kwargs)` | Start an agent turn and wait for the result. |
| `ctx.call_tool(tool, method, args)` | Call a tool through the workflow-host `centaur-tools call` bridge. |
| `ctx.post_to_slack(channel, text, **kwargs)` | Post to Slack through the api-rs Slack context path. |
| `ctx._pool` | Access the workflow database pool when the workflow-host sandbox receives `DATABASE_URL`. |

The handler may re-execute after a restart. `ctx.step(...)` memoizes the result
after its callback returns, so writes and external API calls inside a step must
still be idempotent. If the host crashes after the external side effect but
before the checkpoint commits, the step can replay.

These primitives compose into larger automations:

- **Scheduled operations**: run a daily digest, weekly cleanup, periodic sync,
  or business-hours monitor without a human prompt.
- **Polling loops**: sleep between checks for CI, blockchain confirmations,
  billing state, deploy health, or vendor exports.
- **Event-driven flows**: expose a signed webhook and let the handler process a
  normalized webhook envelope.
- **Agent orchestration**: use agents for judgment-heavy steps while the
  workflow owns timing, retries, state, and final delivery.

## Run a workflow

Create a run through the API:

```bash
curl -s "$CENTAUR_API_URL/api/workflows/runs" \
  -H "Content-Type: application/json" \
  -d '{
    "workflow_name": "nightly_report",
    "input": {"channel": "ops", "topic": "open incidents"},
    "eager_start": true
  }' | jq
```

Inspect it:

```bash
curl -s "$CENTAUR_API_URL/api/workflows/runs/$RUN_ID" | jq
```

## Schedule a workflow

Workflows can run from schedule metadata declared beside the handler. Use this
when a workflow should be started by the platform on a clock instead of by an
API call or webhook.

```python
WORKFLOW_NAME = "daily_market_digest"

SCHEDULE = {
    "type": "cron",
    "cron": "0 9 * * 1-5",
    "timezone": "America/New_York",
    "input": {
        "channel": "markets",
        "topic": "overnight market structure and portfolio-relevant news",
    },
}
```

Cron schedules use five fields:

```text
minute hour day-of-month month day-of-week
```

Examples:

| Cron | Meaning |
|------|---------|
| `0 9 * * 1-5` | 9:00 AM every weekday. |
| `*/15 * * * *` | Every 15 minutes. |
| `30 6 * * *` | 6:30 AM every day. |
| `0 0 1 * *` | Midnight on the first day of every month. |

Always set `timezone` for human-facing schedules. Without an explicit timezone,
cron expressions are easy to misread across daylight saving changes and
deployments in different regions.

Use `input` to keep the handler deterministic for scheduled runs: channel names,
query scopes, tenant IDs, lookback windows, and delivery settings should be
declared in the schedule instead of inferred from wall-clock state when
possible.

For workflows that may run longer than their schedule interval, make each tick
idempotent. Put writes and external API calls in named `ctx.step(...)` blocks
only when they use stable provider-side idempotency keys or upserts, derive
those keys from the scheduled window, and have the handler detect
already-processed periods before starting expensive work.

Interval schedules are useful when exact wall-clock alignment does not matter:

```python
SCHEDULE = {
    "type": "interval",
    "seconds": 300,
    "input": {"target": "production"},
}
```

Use cron for calendar semantics such as "weekday at 9 AM"; use intervals for
continuous monitors such as "check every five minutes".

## Expose a workflow as a webhook

Workflows are private unless the workflow file explicitly exports `WEBHOOKS`.
Each webhook is mounted at `POST /api/webhooks/{slug}` and creates a durable
workflow run with a normalized webhook envelope. Use this for provider-driven
entrypoints such as GitHub issue triage, billing events, or deploy callbacks.

```python
from typing import Any

from api.workflow_engine import WorkflowContext


WORKFLOW_NAME = "github_issue_triage"

WEBHOOKS = [
    {
        "slug": "github-issue-triage",
        "provider": "github",
        "auth": {"type": "github", "secret_ref": "GITHUB_WEBHOOK_SECRET"},
        "trigger_key": {"type": "header", "header": "X-GitHub-Delivery"},
        "allowed_methods": ["POST"],
        "allowed_content_types": [
            "application/json",
            "application/x-www-form-urlencoded",
        ],
        "filter": {
            "all": [
                {"source": "header", "key": "x-github-event", "op": "equals", "value": "issues"},
                {"source": "body", "key": "action", "op": "in", "values": ["opened", "reopened"]},
            ]
        },
    }
]


async def handler(inp: dict[str, Any], ctx: WorkflowContext) -> dict[str, Any]:
    webhook = inp["webhook"]
    headers = webhook["headers"]
    payload = webhook["body"]

    issue = payload["issue"]
    repo = payload["repository"]["full_name"]
    result = await ctx.agent_turn(
        f"Triage GitHub issue {repo}#{issue['number']}: {issue['title']}",
        thread_key=f"github:{repo}:{issue['number']}",
    )
    return {"triaged": True, "agent_result": result}
```

Configure the provider to call:

```text
https://<your-centaur-host>/api/webhooks/github-issue-triage
```

For GitHub, set the webhook secret to the same value as
`GITHUB_WEBHOOK_SECRET` in the API deployment and select `application/json`.
GitHub's default `application/x-www-form-urlencoded` payloads also work when
that content type is listed in `allowed_content_types`.

Use `filter` for provider events that can be rejected from headers or JSON body
fields. The API evaluates the filter before creating a workflow run, which
keeps org-wide webhooks from spawning a sandbox for events the handler would
immediately skip.

Webhook requests do not use Centaur API keys. The API verifies the provider
signature before creating workflow state. Use
`{"type": "github", "secret_ref": "GITHUB_WEBHOOK_SECRET"}` for GitHub
`X-Hub-Signature-256` webhooks, or `{"type": "hmac", ...}` for other SHA-256
HMAC providers. During local development or for trusted internal routes, use
`{"type": "none"}`.

The workflow receives input in this shape:

```json
{
  "webhook": {
    "slug": "github-issue-triage",
    "provider": "github",
    "method": "POST",
    "path": "/api/webhooks/github-issue-triage",
    "headers": {
      "x-github-event": "issues",
      "x-github-delivery": "..."
    },
    "query": {},
    "body": {},
    "raw_body_sha256": "...",
    "source_ip": "203.0.113.10"
  }
}
```

Sensitive headers such as signatures, cookies, authorization, and API keys are
removed before the workflow input is persisted. `trigger_key` controls
idempotency; prefer a provider delivery header like `X-GitHub-Delivery`. If no
trigger key is configured, Centaur uses the raw body SHA-256 hash.

The webhook endpoint returns `202` when it creates a new run and `200` when the
same trigger key maps to an existing run.

## Verify

After deploying an overlay, check API logs for workflow load events and create a
small run with `eager_start: true`. If the workflow is missing, inspect
`WORKFLOW_DIRS`, the configured repo/ref in repo-cache, and whether the file exports
`WORKFLOW_NAME`. For webhooks, also check for
`workflow_webhook_registered` in the API logs and send a signed request to the
public `/api/webhooks/{slug}` URL.

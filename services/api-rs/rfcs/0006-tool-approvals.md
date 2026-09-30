# RFC 0006: Workflow-native approvals

Status: Implemented, opt-in; credentialed end-to-end proof required before activation.

## Boundary

This is an explicit guarded-action API, not interception of arbitrary tool or
outbound HTTP calls. An ordinary sandbox uses `centaur-console approvals`; only the
approval-only workflow executor has the provider credential grant. Hiding a
tool or putting an instruction in the prompt is not enforcement.

The implementation reuses native workflow queues, durable event waits, signed
workflow buttons, Slack transport, workflow-host sandboxes and
`WORKFLOW_PRINCIPAL`. It adds one durable authorization record, not another
scheduler, Slack delivery worker or tool-host runtime.

## Lifecycle

1. Console authenticates the current sandbox proxy entitlement and attests its
   sandbox/principal IDs. API admission resolves exactly one running, home-team
   Slack execution, its original thread and requester. Callers cannot choose
   the destination, executor or approving actor.
2. Core validates the action policy and registered executor. It freezes the full
   JSON arguments, policy hash and payload hash. Request insertion and enqueue
   of the native approval driver commit in one PostgreSQL transaction.
3. The driver posts plaintext payload sections and signed native
   `centaur.workflow.action:` buttons. It stores the original message timestamp
   and complete signed card, then suspends on a durable event.
4. Existing Slack ingress verifies Slack's webhook signature. The native
   workflow-action endpoint verifies the button signature. Approval decisions
   additionally require the Slack ingress service identity; ordinary workflow
   input and its `click.user_id` field are never authority.
5. Core checks current policy hash, team, channel, original message/card, payload
   hash, expiry, allowlisted actor and self-approval policy. One locked transition
   chooses the decision; the update and native wake event are atomic.
6. Immediately before execution, core verifies enablement, approval-only marker
   and exact registered principal. One compare-and-set commits
   `approved -> executing`. The existing workflow host runs the pinned executor
   with only the frozen arguments and its workflow-scoped proxy credentials.
7. The driver stores a redacted outcome, optionally including explicitly public
   result fields, and updates Slack through native checkpoints. `succeeded`
   means the executor returned successfully, not that an external asynchronous
   business workflow finished.

Slack authenticates an app webhook containing an actor ID; it does not provide
a user-held cryptographic signature. See
[Slack request verification](https://docs.slack.dev/authentication/verifying-requests-from-slack/).

## Executor contract

```python
WORKFLOW_NAME = "guarded_ticket_create"
WORKFLOW_PRINCIPAL = True
WORKFLOW_REQUIRES_APPROVAL = True

async def handler(inp, ctx):
    return await ctx.call_tool("tickets", "create_ticket", inp)
```

Direct API, button, schedule and child-workflow starts cannot execute an
approval-only workflow. Reserved driver names are also blocked from public
starts, and the driver checks its task ID against the request's owner. A forged
approval object does not unlock an executor. Marker discovery requires a scoped
principal and sandboxed workflow hosting. The approved host receives
`CENTAUR_APPROVAL_PROTOCOL=workflow-tool-approvals-v1`; ordinary hosts receive
an empty value. Deployment adapters may require this to reject older cores.

Executor principal IDs and declared aliases are reserved across runtime clones,
including when an approval-only workflow is disabled. Ordinary agent sessions,
generic tool hosts and other workflow hosts cannot select them. Reservations
are monotonic per process; removing an action never frees its credential for
ordinary execution during that process's lifetime. Before activation, remove
any pre-existing ordinary sessions/proxies using the executor principal.

Install the protected tool and `centaur-tools` shim in the workflow sandbox.
The privileged path refuses the API-side `WORKFLOW_TOOL_API_URL` fallback.
Protected executors must be short, single-dispatch workflows, without durable
sleeps, human waits or arbitrary agents/code execution. A suspension or failure
after claim is uncertain and is not resumed. Put long-running business work
behind a provider-side workflow and approve its start operation.

## Policy

Default `CENTAUR_TOOL_APPROVAL_POLICIES={}` grants no actions. Example:

```json
{
  "tickets-create": {
    "workflow": "guarded_ticket_create",
    "requester_principals": ["prn_Conversation"],
    "team_id": "TEXAMPLE",
    "approver_user_ids": ["UREVIEWER"],
    "allow_self_approval": false,
    "expires_seconds": 3600,
    "timeout_seconds": 120,
    "implementation_revision": "reviewed-immutable-release"
  }
}
```

Admission resolves the executor OID from the workflow's `WORKFLOW_PRINCIPAL`
registry entry and freezes it in the payload and policy hash. Missing registry
entries, disabled workflows and requester/executor overlap fail closed. An
optional `executor_principal` field can pin the expected OID for compatibility;
a mismatched pin is unavailable. A changed registry binding invalidates pending
approvals rather than transferring them to the new principal. Never reuse an
executor principal for ordinary sessions or unrelated workflows.
Remove the provider mutation capability from ordinary principals,
default roles, requester OAuth grants and alternate usable routes. Grant only
the required provider host/path/method/key to the executor. Keep Slack bot tokens
in infrastructure, not either sandbox.

Pin the executor image, tool code and workflow definitions to reviewed immutable
revisions. `implementation_revision` is an audit/policy binding, not code
attestation. Change it whenever the operation changes. Do not hot-reload
protected code with approvals pending. Policies are startup configuration:
revocation is effective only after all API replicas use the updated policy.

No new Slackbot flag or interaction prefix is needed. Shared ingress receivers
must already forward the native workflow action prefix and original signed
webhook bytes. Configure API policy through the chart's existing extra-env
facility; no chart default enables this feature.

### Deliberately public results

`public_result_fields` is an optional list of up to eight distinct top-level
field names. It defaults to `[]`, preserving output redaction for existing
actions. Each selected value must be a string, and the whole selected object
must fit in 2,000 ASCII-escaped display bytes. Nested objects, missing fields,
non-string values and oversized output cause the entire public projection to
be omitted, with `output_omitted: true`; execution still counts as succeeded
and must not be repeated merely to recover output.

Selected fields are persisted as `result.output`, returned to the requesting
bot by the CLI/status API, and displayed as plaintext in the final Slack card.
All unselected fields and all executor errors remain private. This is an
explicit publication policy, **not a secret scanner**: only select fields that
reviewed executor code guarantees are non-secret. The field list is displayed
on the approval card and bound into both the payload and policy hashes.

### Credential-free hello-world test

`workflows/approval_hello_world.py` is an approval-only executor that accepts
exactly `{}` and returns `{"message":"Hello world!"}`. It calls no tools or
external services and needs no provider credential grants. It has no schedule
or webhook. Without a configured action policy it cannot be requested, and
the runtime marker check rejects ordinary/older workflow hosts.

To enable it on a reviewed test deployment:

1. Install the approval-capable core and this workflow in the workflow-host
   image/source bundle. Keep workflow-host sandboxing enabled. If using
   `WORKFLOW_ENABLE_MODE=allowlist`, add `approval_hello_world` to the existing
   `WORKFLOW_ALLOWED_NAMES` value without removing other entries.
2. Discover/register the workflow's dedicated `workflow-approval-hello-world`
   principal; no manual executor OID lookup is needed. Do not give it provider
   secrets or ordinary-agent access. The requester
   OID is the principal assigned to the bot session's sandbox, not the human
   Slack user ID.
3. Merge the following action into `CENTAUR_TOOL_APPROVAL_POLICIES`, replacing
   every placeholder with the reviewed deployment values. Roll out the API
   configuration so every replica starts with the same policy.

```json
{
  "hello-world": {
    "workflow": "approval_hello_world",
    "requester_principals": ["prn_Conversation"],
    "team_id": "TEXAMPLE",
    "approver_user_ids": ["UREVIEWER"],
    "allow_self_approval": false,
    "expires_seconds": 600,
    "timeout_seconds": 60,
    "implementation_revision": "reviewed-immutable-release",
    "public_result_fields": ["message"]
  }
}
```

From a running bot session in a Slack thread:

```sh
centaur-console approvals call hello-world --arguments '{}'
```

Or ask the bot: “Run the `hello-world` approval test with empty arguments and
tell me the returned message after approval.” An allowlisted reviewer other
than the requester clicks Accept. The final Slack card then includes
`{"message":"Hello world!"}`, and the command returns
`{"outcome":"succeeded","output":{"message":"Hello world!"}}` inside
`result`. For a deliberately single-person test, explicitly set
`allow_self_approval: true` and include that person's Slack ID; never silently
relax the default. Decline, expiry and unauthorized clicks must not produce a
greeting or execute the handler.

The command waits by default. If the agent turn ends or its wait times out,
the request remains durable; inspect it with `centaur-console approvals status <id>`
from the same sandbox instead of resubmitting. Approval completion updates
Slack, but does not automatically start a new agent turn.

This tests the existing Centaur approval path only. It does not provision AWS
resources or implement external execution services or signed approval grants.

Test checklist (use a fresh request for each decision):

- Run `centaur-console approvals actions` from the active Slack bot execution and check
  that `hello-world` is listed. Do not run the command from an ordinary laptop
  shell; it needs the sandbox's proxy entitlement and active Slack execution.
- Before approval, the card must show `{}` and `public_result_fields: ["message"]`,
  but no completed greeting. Accept as an allowlisted reviewer and verify both
  the final card and CLI result contain `Hello world!`.
- Decline a fresh request: it must finish as `declined` with no greeting.
- Have someone outside the approver list click Accept: the request must remain
  pending and must not run. An allowed reviewer can still decide it.
- Submit with `--no-wait`, then call `centaur-console approvals cancel <id>` before
  approval: status must become `cancelled` and later clicks must not execute it.
- To test expiry quickly, configure `expires_seconds: 30` before submitting a
  fresh request. Leave it untouched and allow another 30 seconds for the
  durable expiry check. It must become `expired` without a greeting.
- Repeated status reads or duplicate decision deliveries must not execute the
  handler again. Direct generic starts of `approval_hello_world` must be rejected.

## Recovery and cancellation

Requests remain pending after the originating agent turn ends. Only admission
requires an active execution. Request ownership for status/cancel remains
sandbox/principal-scoped; losing that sandbox may require operator inspection.
`centaur-console approvals cancel <id>` cancels pending or approved requests. Cancellation
after the execution claim cannot undo the external operation.

Pending/approved requests expire or cancel on changed/removed policy. Native
event waits wake at most every 30 seconds to recheck. A committed execution claim
is never retried. If the driver resumes after a crash with status `executing`,
it records `unknown`. Timeouts, executor failures and ambiguous provider
responses also become `unknown`. Inspect the provider before resubmitting.
Use stable provider request/workflow IDs and reject duplicate operations where
available; this protocol does not promise exactly-once external effects.

Slack post/checkpoint failure can leave a duplicate card. Only the recorded
original message/card can authorize the request. Final-message delivery retries
do not invoke the executor again.

Approval status includes `workflow_task_id` and `workflow_run_id` for the native
workflow history. Cancelling that native run also reconciles its approval:
unclaimed requests become `cancelled`, claimed requests become `unknown`, and
known terminal outcomes are preserved. Native cancellation commits first;
reconciliation is recoverable if the process dies between commits. The execution
claim checks and locks native task state, so already-cancelled tasks cannot
obtain execution authority.

Startup, the existing workflow maintenance loop, status reads, decision handling
and execution claims reconcile from persisted task state. Exhausted/removed
tasks and elapsed execution deadlines cannot leave a request actionable forever.
If `WORKFLOW_RECONCILE_INTERVAL_SECS=0`, startup and request-path reconciliation
still operate, but there is no periodic background repair. Reconciliation never
replays an executor or replaces a known result. A force-cancelled/failed native
driver cannot deliver its final Slack edit; its card may be stale, but subsequent
clicks are rejected and the status API reports the reconciled outcome. Use the
approval-specific cancel command when possible so the driver can deliver its
normal final card. Retain request rows and linked native IDs for audit.
Only explicitly published result fields are returned to Slack or the requester;
raw executor output/errors are not.

Migration 0056 supersedes the earlier draft's delivery/claim columns and cancels
old pending/approved records; old executing records become unknown. Stop the
old approval workers before this migration. Mixed old/new approval-worker
versions are not supported. Never replay those old requests automatically.

## Payload and CLI

Approvals are part of the existing `centaur-console` tool package and client.
Install/allowlist `centaur-console`; there is no separate approval package.
The legacy `centaur-approvals` command is a compatibility entry point to the same
command group, not a second implementation. Refresh installed tool shims on
upgrade to replace the old standalone package's command.

```sh
centaur-console approvals actions
centaur-console approvals call tickets-create --arguments '{"title":"Example","body":"Complete content"}'
centaur-console approvals status <request-uuid>
centaur-console approvals cancel <request-uuid>
```

The CLI prints an ID and waits by default. `--no-wait` returns after admission.
Client timeout does not cancel. Reuse `--idempotency-key <UUID>` only for identical
admission within the same execution. Admission is capped at 20 requests/execution.

The complete argument object, workflow, executor principal and implementation
revision must fit in 12,000 displayed JSON bytes; oversized payloads are rejected,
never truncated. Plaintext sections preserve large integers and use JSON escapes
for Unicode and Slack delimiters. Everyone in the thread can read the arguments.
Never include secrets. Do not approve deferred mutable URLs/files/draft IDs;
snapshot their contents first. The approval covers the reviewed executor's
semantics, not each HTTP request made by arbitrary downstream code.

## Validation and activation

Unit tests cover policy validation, complete display, native signatures and
workflow declaration checks. The database test checks atomic native admission,
idempotency, actor/card/message boundaries, competing decisions, expiry,
cancellation, execution claims, forged driver ownership and uncertain recovery:

```sh
TOOL_APPROVAL_TEST_DATABASE_URL=postgres://localhost/disposable \
  cargo test -p centaur-workflows approvals
```

Use a disposable database: tests install Absurd and create/drop a unique test
schema. Before enabling credentials, prove the complete local path through
Console, real sandbox proxies, Slack, the isolated executor and a controlled
provider. Include unauthorized clicks, direct-call bypass, duplicates, expiry
and restart. No production enablement is implied by source or unit-test success.

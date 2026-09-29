# RFC 0006: Approval-gated tool calls

Status: Implemented, opt-in; deployment proof required before enabling actions

An agent requests a configured tool method through `centaur-approvals`. The
originating Slack thread receives the complete JSON arguments with Accept and
Decline buttons. Only the policy's listed Slack users can decide. Accepting one
request authorizes that invocation once; it does not give the agent credentials.

## Ownership and trust boundaries

The tool calls Console's `/api/v1/sandbox/tool_approvals` endpoints. The existing
sandbox entitlement authenticates the current proxy assignment; Console forwards
the verified sandbox and principal IDs using its service identity. api-rs checks
the durable session and its running execution. The tool supplies an expected
execution ID, action name, JSON object and UUID idempotency key. It cannot supply
an executor principal, Slack destination, requester or approver identity.

api-rs stores the request in `tool_approvals` (migration 0055). Its repository
serializes admission against the source execution, rejects changed arguments
under the same key and limits each execution to 20 requests. The policy binds a
tool/method, separate executor principal, requester principals, home Slack team,
approvers, lifetime and implementation revision. Payload and policy hashes are
stored with the immutable request. Both the request and decision require a
home-workspace Slack execution; Console sessions and Slack Connect requesters
without a verified home-team identity are ineligible in this first version.

Slackbot leases durable delivery work, posts in the source thread and records
the message timestamp and full card. Its existing signed Chat SDK ingress
forwards decisions. API decision endpoints admit only the Slack ingress caller;
request/read endpoints admit only Console. The broker checks workspace, channel,
message, card contents, payload hash, actor allowlist, self-approval policy,
expiry, active source execution and current policy hash. A conditional update
makes the first valid decision win. Replayed or copied buttons cannot authorize
another request. Unauthorized/stale clicks get private unavailable feedback.

Slack's signature authenticates an app webhook containing the clicking user's
identity; it is not a user-held cryptographic signature. See
[Slack request verification](https://docs.slack.dev/authentication/verifying-requests-from-slack/).

## Execution and recovery

The API worker cancels pending/approved requests when the source execution ends,
its sandbox/principal assignment changes, or its configured policy changes. It
expires unanswered requests. Immediately before dispatch it locks the source
execution and commits `approved -> executing`; cancellation after that point may
be too late to stop an external side effect.

Execution uses the existing isolated tool host with the configured executor
principal. The tool host receives only the stored method and arguments. A stable
`approval-call-<UUID>` idempotency key correlates its durable session execution.
The approval worker never reclaims an executing request. A crash/timeout becomes
`unknown`, not a retry. Successful output is stored, returned to the waiting CLI,
and the Slack card is updated. Tool failures may have partial effects; inspect
the provider before submitting another request. Provider idempotency should also
be used by each protected integration where available. Exactly-once external
effects are not promised by this protocol.

Delivery uses leases and revision receipts, so restarts replay unfinished card
updates. A Slack post with a lost response can leave a duplicate card; only the
recorded message accepts decisions. A recorded card deleted from Slack leaves
delivery retrying until an operator investigates. Neither situation retries a
tool invocation.

## Configure a protected action

The default API policy is `{}` and Slack delivery is disabled. Set
`CENTAUR_TOOL_APPROVAL_POLICIES` on api-rs to a JSON object, for example:

```json
{
  "tickets-create": {
    "tool": "tickets",
    "method": "create_ticket",
    "executor_principal": "prn_Executor",
    "requester_principals": ["prn_Conversation"],
    "team_id": "TEXAMPLE",
    "approver_user_ids": ["UREVIEWER"],
    "allow_self_approval": false,
    "expires_seconds": 600,
    "timeout_seconds": 120,
    "implementation_revision": "reviewed-immutable-release"
  }
}
```

Set `SLACKBOTV2_TOOL_APPROVALS_ENABLED=true` on Slackbot. The chart's existing
`apiRs.extraEnv` and `slackbotv2.extraEnv` support these values. A shared Slack
interactivity receiver must forward the original signed bytes for the reserved
`centaur.tool-approval:<UUID>:approved|declined` action IDs. It must not make the
authorization decision itself.

Provision a dedicated executor principal with only the protected integration's
required grants. Remove that write capability from ordinary sandbox principals,
their default roles, requester OAuth credentials and any other usable route.
Hiding a CLI or asking the harness to use the proxy is insufficient enforcement.
Do not grant Slack bot tokens to either principal. Verify directly from a real
ordinary sandbox that the provider mutation fails without approval.

Pin the executor's image/tool repositories to the reviewed immutable release and
set `implementation_revision` to identify that release. This field binds policy
and audit records; it does not itself pin a repository or attest installed code.
Do not hot-reload protected tool code while approvals are pending. Update the
policy revision whenever the implementation changes, and complete the policy
rollout before treating an approver removal as effective across replicas.

Only expose reviewed methods whose entire operation is described by JSON values.
Do not configure arbitrary shell/code runners or methods that interpret paths,
URLs or mutable draft IDs as deferred request content. Such integrations need a
prepare/commit adapter that snapshots the content before approval. Credentials
come from the executor proxy and must never be included in arguments or output.
The first version approves a tool invocation, not every HTTP request the tool
may make internally. Live Slack user-group membership is not consulted; deploy
the resolved user allowlist explicitly.

## Agent use

```sh
centaur-approvals actions
centaur-approvals call tickets-create --arguments '{"title":"Example","body":"Complete content"}'
centaur-approvals status <request-uuid>
```

The CLI prints the request ID immediately and waits by default. `--no-wait`
returns after admission. A CLI timeout does not cancel the durable request;
check its status instead of resubmitting. Use `--idempotency-key <UUID>` when
retrying the same admission within the same execution.

Full payloads must fit in 12,000 bytes of displayed JSON. Oversized requests are
rejected before posting, never truncated. Unicode and Slack delimiters use JSON
escapes so invisible/bidirectional characters and mention syntax remain visible.
Slack receives the exact frozen serialization as plaintext sections, preserving
large integers and escaping. Everyone able to read the thread can read the
arguments, so only route payloads appropriate for that channel.

## Validation

Focused tests cover policy/actor checks, card tampering, large-number rendering,
signed ingress and retry, Console identity attestation, admission idempotency,
competing decisions, execution claims, expiry, cancellation and crash recovery.
The database test uses a unique temporary schema and the base session migrations
plus 0055; it does not require unrelated search/vector extensions:

```sh
TOOL_APPROVAL_TEST_DATABASE_URL=postgres://localhost/disposable \
  cargo test -p centaur-api-server tool_approvals
```

Before enabling any credentialed action, prove the full local path through
Console, the ordinary sandbox proxy, Slack, the isolated executor and a controlled
provider. Include an unauthorized approver, direct-call bypass attempt, duplicate
click, altered card, expired request and API restart. Production policy remains
empty until that action's credential isolation and pinned implementation are
verified.

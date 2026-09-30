-- Approval is a one-shot authorization for immutable tool arguments. The
-- executor claim is never reclaimed: an uncertain outcome requires inspection.
create table tool_approvals (
    id uuid primary key,
    execution_id text not null references session_executions(execution_id) on delete cascade,
    thread_key text not null references sessions(thread_key) on delete cascade,
    sandbox_id text not null,
    principal_id text not null,
    executor_principal text not null,
    idempotency_key uuid not null,
    action text not null,
    policy_hash text not null,
    payload jsonb not null,
    payload_json text not null check (octet_length(payload_json) <= 12000),
    payload_hash text not null,
    team_id text not null,
    channel_id text not null,
    thread_ts text not null,
    requester_id text not null,
    status text not null default 'pending' check (status in
        ('pending', 'approved', 'declined', 'expired', 'cancelled', 'executing', 'succeeded', 'failed', 'unknown')),
    decided_by text,
    decided_at timestamptz,
    created_at timestamptz not null default now(),
    expires_at timestamptz not null,
    execution_deadline timestamptz,
    result jsonb,
    revision bigint not null default 1,
    message_ts text,
    message_blocks jsonb,
    delivered_revision bigint not null default 0,
    delivery_token uuid,
    delivery_until timestamptz,
    unique (execution_id, idempotency_key),
    check (octet_length(payload::text) <= 20000)
);
create index tool_approvals_work on tool_approvals(status, created_at);
create unique index tool_approvals_one_executing_per_executor
    on tool_approvals(executor_principal) where status = 'executing';
create index tool_approvals_delivery on tool_approvals(team_id, created_at)
    where delivered_revision < revision;

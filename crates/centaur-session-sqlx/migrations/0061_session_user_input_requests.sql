create table session_user_input_requests (
    request_id text primary key,
    thread_key text not null references sessions(thread_key) on delete cascade,
    execution_id text not null references session_executions(execution_id) on delete cascade,
    requester_user_id text,
    questions jsonb not null,
    status text not null default 'pending'
        check (status in ('pending', 'submitted', 'resolved', 'cancelled')),
    answers jsonb,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create index session_user_input_requests_pending
    on session_user_input_requests(thread_key, execution_id)
    where status in ('pending', 'submitted');

-- Every terminal transition closes questions, including failure/recovery paths.
-- Use the same thread event lock as the repository so replay cursors remain safe.
create function cancel_execution_user_input_requests() returns trigger
language plpgsql as $$
declare
    request record;
begin
    if new.status in ('completed', 'failed', 'cancelled') and old.status <> new.status then
        for request in
            update session_user_input_requests
            set status = 'cancelled', updated_at = now()
            where execution_id = new.execution_id and status in ('pending', 'submitted')
            returning request_id, thread_key
        loop
            perform pg_advisory_xact_lock(
                hashtextextended('centaur:session-events:' || request.thread_key, 0)
            );
            insert into session_events (thread_key, execution_id, event_type, payload)
            values (request.thread_key, new.execution_id, 'session.user_input.resolved',
                jsonb_build_object('request_id', request.request_id,
                    'execution_id', new.execution_id, 'status', 'cancelled',
                    'reason', 'execution_ended'));
        end loop;
    end if;
    return new;
end;
$$;

create trigger session_execution_user_input_terminal
    after update of status on session_executions
    for each row execute function cancel_execution_user_input_requests();

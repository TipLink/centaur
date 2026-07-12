alter table sessions
    add column if not exists sandbox_content_revision text;

comment on column sessions.sandbox_content_revision is
    'Assignment-bound digest of the immutable deployment boot-content generation and sandbox ID; NULL on legacy assignments.';

create or replace view centaur_readonly_sessions as
select
    thread_key,
    sandbox_id,
    harness_type,
    harness_thread_id,
    persona_id,
    status,
    metadata ->> 'source' as source,
    metadata ->> 'platform' as platform,
    metadata ->> 'thread_id' as external_thread_id,
    created_at,
    updated_at,
    sandbox_content_revision
from sessions;

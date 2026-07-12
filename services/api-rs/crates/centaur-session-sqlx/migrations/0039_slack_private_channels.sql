alter table slack_sync_channels
    add column if not exists is_private boolean;

-- Existing rows predate the dedicated privacy column. Trust an explicit
-- boolean from the stored Slack payload; when privacy is absent or malformed,
-- fail closed until a live Slack sync can classify the channel.
update slack_sync_channels
set is_private = case
    when jsonb_typeof(raw_payload -> 'is_private') = 'boolean'
        then (raw_payload ->> 'is_private')::boolean
    else true
end;

alter table slack_sync_channels
    alter column is_private set default true,
    alter column is_private set not null;

create index if not exists idx_slack_sync_channels_private
    on slack_sync_channels (is_private, channel_id);

drop policy if exists centaur_readonly_slack_sync_channels_select
    on slack_sync_channels;
create policy centaur_readonly_slack_sync_channels_select
    on slack_sync_channels
    for select
    to centaur_readonly
    using (
        not is_private
        or channel_id = centaur_current_slack_channel_id()
    );

drop policy if exists centaur_readonly_slack_sync_message_attachments_select
    on slack_sync_message_attachments;
create policy centaur_readonly_slack_sync_message_attachments_select
    on slack_sync_message_attachments
    for select
    to centaur_readonly
    using (
        exists (
            select 1
            from slack_sync_channels channels
            where channels.channel_id = slack_sync_message_attachments.channel_id
        )
    );

drop policy if exists centaur_readonly_slack_sync_messages_select
    on slack_sync_messages;
create policy centaur_readonly_slack_sync_messages_select
    on slack_sync_messages
    for select
    to centaur_readonly
    using (
        exists (
            select 1
            from slack_sync_channels channels
            where channels.channel_id = slack_sync_messages.channel_id
        )
    );

drop policy if exists centaur_readonly_company_context_documents_select
    on company_context_documents;
create policy centaur_readonly_company_context_documents_select
    on company_context_documents
    for select
    to centaur_readonly
    using (
        source <> 'slack'
        or exists (
            select 1
            from slack_sync_channels channels
            where channels.channel_id = metadata ->> 'channel_id'
        )
    );

-- Fineas company context intentionally exposes documents from public,
-- syncable Slack channels across channel-scoped principals. Keep direct access
-- to the principal's current channel (including a private channel), but never
-- use the Slack channel-id prefix as a privacy signal.
create or replace function centaur_slack_channel_is_public_syncable(
    _schema name,
    _channel_id text
)
returns boolean
language plpgsql
stable
security definer
set search_path = pg_catalog
as $$
declare
    public_syncable boolean;
begin
    execute format(
        'select exists (
            select 1
            from %I.slack_sync_channels channels
            where channels.channel_id = $1
              and channels.is_syncable
              and not channels.is_private
        )',
        _schema
    )
    into public_syncable
    using _channel_id;
    return coalesce(public_syncable, false);
end
$$;

revoke all on function centaur_slack_channel_is_public_syncable(name, text)
    from public;
grant execute on function centaur_slack_channel_is_public_syncable(name, text)
    to centaur_slack_reader;

drop policy if exists centaur_context_docs_reader_select
    on company_context_documents;
create policy centaur_context_docs_reader_select
    on company_context_documents
    for select
    to centaur_slack_reader
    using (
        source <> 'slack'
        or metadata ->> 'channel_id' = centaur_current_slack_channel_id()
        or centaur_slack_channel_is_public_syncable(
            current_schema(),
            metadata ->> 'channel_id'
        )
    );

-- The old helper treated a C-prefixed id as public and was executable by
-- PUBLIC. Its only policy dependency was replaced immediately above.
drop function if exists centaur_slack_channel_is_syncable(name, text);

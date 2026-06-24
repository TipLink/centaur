drop policy if exists centaur_context_docs_reader_select on company_context_documents;
create policy centaur_context_docs_reader_select
    on company_context_documents
    for select
    to centaur_slack_reader
    using (
        source <> 'slack'
        or metadata ->> 'channel_id' = nullif(current_setting('centaur.slack_channel_id', true), '')
        or (
            metadata ->> 'channel_id' like 'C%'
            and exists (
                select 1
                from slack_sync_channels channels
                where channels.channel_id = metadata ->> 'channel_id'
                  and channels.is_syncable
            )
        )
    );

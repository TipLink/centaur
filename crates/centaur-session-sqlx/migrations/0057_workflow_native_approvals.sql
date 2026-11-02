-- Supersede the opt-in tool-host prototype without replaying any old request.
update tool_approvals set status = case when status = 'executing' then 'unknown' else 'cancelled' end
where status in ('pending', 'approved', 'executing');
alter table tool_approvals
    add column workflow_task_id uuid,
    add column workflow_run_id uuid,
    drop column delivery_token,
    drop column delivery_until,
    drop column delivered_revision,
    drop column revision;
drop index tool_approvals_one_executing_per_executor;
create unique index tool_approvals_workflow_task on tool_approvals(workflow_task_id);

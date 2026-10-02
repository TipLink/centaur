use super::repository::Record;
use super::*;

const MAX_PUBLIC_OUTPUT_BYTES: usize = 2000;

/// This is an explicit publication allowlist, not an automatic secret scanner.
/// Malformed/missing/oversized fields suppress the entire public output.
fn public_output(policy: &ActionPolicy, raw: &Value) -> Option<Value> {
    let mut selected = serde_json::Map::new();
    for field in &policy.public_result_fields {
        let text = raw.as_object()?.get(field)?.as_str()?;
        if text.len() > MAX_PUBLIC_OUTPUT_BYTES {
            return None;
        }
        selected.insert(field.clone(), json!(text));
    }
    let output = Value::Object(selected);
    (display_json(&output).ok()?.len() <= MAX_PUBLIC_OUTPUT_BYTES).then_some(output)
}

pub(super) fn executor_outcome(
    policy: &ActionPolicy,
    result: Result<Value, WorkflowRuntimeError>,
) -> (&'static str, Value) {
    let Ok(raw) = result else {
        return ("unknown", json!({"outcome":"unknown"}));
    };
    let mut result = json!({"outcome":"succeeded"});
    if !policy.public_result_fields.is_empty() {
        if let Some(output) = public_output(policy, &raw) {
            result["output"] = output;
        } else {
            // Execution succeeded; missing public output is not a reason to
            // repeat a potentially side-effecting operation.
            result["output_omitted"] = json!(true);
        }
    }
    ("succeeded", result)
}

pub(super) fn final_message(
    id: Uuid,
    action: &str,
    status: &str,
    result: Option<&Value>,
    policy: Option<&ActionPolicy>,
) -> Value {
    let (label, detail) = match status {
        "succeeded" => ("Completed", "The action completed successfully."),
        "declined" => (
            "Declined",
            "The action did not run. Start a new request if you want to approve it.",
        ),
        "expired" => (
            "Expired",
            "The request expired before the action ran. Start a new request to try again.",
        ),
        "cancelled" => (
            "Cancelled",
            "The action did not run. Start a new request if it is still needed.",
        ),
        "unknown" => (
            "Outcome uncertain",
            "The action may have run. Inspect the provider outcome before submitting another request.",
        ),
        _ => ("Processing", "The request is still being processed."),
    };
    let title = format!("{label}: {action}");
    let mut text = format!("{title}. {detail}");
    let mut blocks = vec![
        json!({"type":"header","text":{"type":"plain_text","emoji":false,"text":title}}),
        json!({"type":"section","text":{"type":"plain_text","emoji":false,"text":detail}}),
    ];
    if status == "succeeded"
        && let Some(policy) = policy.filter(|p| !p.public_result_fields.is_empty())
        && let Some(raw) = result.and_then(|value| value.get("output"))
        && let Some(output) = public_output(policy, raw)
    {
        // Only the deliberately published projection is persisted here. Never
        // use the raw workflow result in a Slack message or fallback text.
        if let Ok(display) = display_json(&output) {
            blocks.push(preformatted(&display));
            text.push('\n');
            text.push_str(&display);
        }
    }
    if status == "succeeded" && result.is_some_and(|r| r["output_omitted"] == true) {
        let detail = "The result could not be displayed. The action succeeded; do not rerun it just to recover the output.";
        blocks.push(
            json!({"type":"section","text":{"type":"plain_text","emoji":false,"text":detail}}),
        );
        text.push('\n');
        text.push_str(detail);
    }
    blocks.push(
        json!({"type":"context","elements":[{"type":"plain_text","emoji":false,
        "text":format!("Request {id}")}]}),
    );
    json!({"text":text,"blocks":blocks,"mrkdwn":false})
}

fn preformatted(text: &str) -> Value {
    // Native preformatted text preserves indentation without interpreting
    // markdown, mentions or embedded triple backticks from JSON strings.
    json!({"type":"rich_text","elements":[{"type":"rich_text_preformatted","border":0,
        "elements":[{"type":"text","text":text}]}]})
}

pub(super) fn validate_executor(
    p: &ActionPolicy,
    sandbox: Option<&WorkflowHostSandboxRuntime>,
) -> Result<(), WorkflowRuntimeError> {
    WorkflowEnablement::from_env()?.ensure_enabled(&p.workflow)?;
    let sandbox = sandbox.ok_or_else(unavailable)?;
    if !sandbox.requires_approval(&p.workflow)
        || sandbox
            .spec_for_workflow(&p.workflow)?
            .iron_control_principal
            .as_deref()
            != Some(&p.executor_principal)
    {
        return Err(unavailable());
    }
    Ok(())
}

pub(super) fn card(row: &Record) -> Value {
    let mut blocks = vec![
        json!({"type":"header","text":{"type":"plain_text","emoji":false,
            "text":format!("Approval required: {}",row.action)}}),
        json!({"type":"section","fields":[
            {"type":"mrkdwn","verbatim":false,"text":format!("*Requested by*\n<@{}>",row.requester_id)},
            {"type":"mrkdwn","verbatim":false,"text":format!("*Expires*\n<!date^{}^{{date_short_pretty}} at {{time}}|{} UTC>",
                row.expires_at.timestamp(),row.expires_at.format("%Y-%m-%d %H:%M"))}
        ]}),
        json!({"type":"section","text":{"type":"plain_text","emoji":false,
            "text":"Review the request below, then choose Approve or Decline."}}),
    ];
    for chunk in row.payload_json.as_bytes().chunks(2900) {
        blocks.push(preformatted(
            std::str::from_utf8(chunk).expect("display is ASCII"),
        ));
    }
    let buttons = [
        ("approved", "Approve", "primary"),
        ("declined", "Decline", "danger"),
    ]
    .map(|(action, label, style)| {
        json!({"type":"button","text":{"type":"plain_text","text":label},
            "style":style,
            "action_id":format!("centaur.workflow.action:{}:{action}",row.id),
            "value":json!({"workflow_name":DECISION_WORKFLOW,
                "input":{"approval_id":row.id,"payload_hash":row.payload_hash}}).to_string()})
    });
    blocks.push(json!({"type":"actions","elements":buttons}));
    blocks.push(
        json!({"type":"context","elements":[{"type":"plain_text","emoji":false,
        "text":format!("Request {} · Payload SHA-256: {}",row.id,row.payload_hash)}]}),
    );
    json!({"channel":row.channel_id,"thread_ts":row.thread_ts,"client_msg_id":row.id.to_string(),
        "text":format!("Approval requested: {}",row.action),"blocks":blocks,"unfurl_links":false,"unfurl_media":false})
}

pub(crate) async fn run(
    input: WorkflowTaskInput,
    ctx: TaskContext,
    session_runtime: SessionRuntime,
    sandbox: Option<WorkflowHostSandboxRuntime>,
    clients: WorkflowQueueClients,
) -> absurd::Result<WorkflowResult> {
    run_inner(input, ctx, session_runtime, sandbox, clients)
        .await
        .map_err(absurd_error)
}

async fn run_inner(
    input: WorkflowTaskInput,
    ctx: TaskContext,
    session_runtime: SessionRuntime,
    sandbox: Option<WorkflowHostSandboxRuntime>,
    clients: WorkflowQueueClients,
) -> Result<WorkflowResult, WorkflowRuntimeError> {
    let id = input.input["approval_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(unavailable)?;
    let repo = clients.approval_repository();
    let mut row = repo.load(id, ctx.task_id()).await?;
    // A committed execution claim is never retried. A restarted driver can only
    // report uncertainty; the external provider may already have accepted it.
    if row.status == "executing" {
        repo.recover(id).await?;
    }
    let p = repo.policies.get(&row.action);
    repo.refresh(id, p).await?;
    row = repo.load(id, ctx.task_id()).await?;
    if row.status == "pending" && row.message_ts.is_none() {
        let message: Value = ctx
            .step("approval.card", || async {
                let mut message = card(&row);
                let secret = env::var("CENTAUR_JWT_SIGNING_SECRET").unwrap_or_default();
                slack_buttons::sign_message(&mut message, secret.trim().as_bytes())
                    .map_err(absurd_error)?;
                Ok(message)
            })
            .await?;
        let posted: Value = ctx
            .step("approval.post", || async {
                send_slack_request("chat.postMessage", message.clone())
                    .await
                    .map_err(absurd_error)
            })
            .await?;
        let ts = posted["ts"]
            .as_str()
            .filter(|s| timestamp(s))
            .ok_or_else(|| {
                WorkflowRuntimeError::Upstream("Slack did not return a message timestamp".into())
            })?;
        sqlx::query("update tool_approvals set message_ts=$2,message_blocks=$3 where id=$1 and message_ts is null")
            .bind(id).bind(ts).bind(&message["blocks"]).execute(&repo.pool).await?;
    }
    loop {
        repo.refresh(id, p).await?;
        row = repo.load(id, ctx.task_id()).await?;
        if row.status != "pending" {
            break;
        }
        // Durable native event wait; periodic wake bounds policy/expiry checks.
        match ctx
            .await_event::<Value>(
                &format!("{EVENT_PREFIX}{id}"),
                AwaitEventOptions {
                    step_name: Some("approval.decision".into()),
                    timeout: Some(Duration::from_secs(30)),
                },
            )
            .await
        {
            Ok(_) | Err(absurd::Error::Timeout(_)) => {}
            Err(absurd::Error::Suspend) => return Err(WorkflowRuntimeError::Suspend),
            Err(error) => return Err(error.into()),
        }
    }
    if row.status == "approved" {
        let policy = p.ok_or_else(unavailable)?;
        if validate_executor(policy, sandbox.as_ref()).is_err() {
            sqlx::query(
                "update tool_approvals set status='cancelled' where id=$1 and status='approved'",
            )
            .bind(id)
            .execute(&repo.pool)
            .await?;
        } else {
            if let Some(deadline) = repo.claim(id, policy).await? {
                // Arguments are exclusively the frozen DB payload. No click data
                // or caller-supplied principal is forwarded into the executor.
                let execution = WorkflowTaskInput {
                    workflow_name: policy.workflow.clone(),
                    input: row.payload["arguments"].clone(),
                    harness_type: input.harness_type,
                    slack_button_feedback: None,
                };
                let result = run_python_workflow_host_in_sandbox(
                    execution,
                    ctx.clone(),
                    session_runtime,
                    sandbox.ok_or_else(unavailable)?,
                    clients.clone(),
                    Some(execution::ClaimedExecution {
                        repo: &repo,
                        id,
                        policy,
                        deadline,
                    }),
                )
                .await;
                result?;
            }
        }
    }
    repo.refresh(id, p).await?;
    row = repo.load(id, ctx.task_id()).await?;
    if let Some(ts) = &row.message_ts {
        let mut message = final_message(
            row.id,
            &row.action,
            &row.status,
            row.result.as_ref(),
            p.filter(|policy| hash(policy) == row.policy_hash),
        );
        message["channel"] = json!(row.channel_id);
        message["ts"] = json!(ts);
        ctx.step("approval.final", || async {
            send_slack_request("chat.update", message)
                .await
                .map_err(absurd_error)
        })
        .await?;
    }
    Ok(WorkflowResult {
        workflow_name: DRIVER_WORKFLOW.into(),
        run_id: ctx.run_id().into(),
        task_id: ctx.task_id().into(),
        steps: vec!["approval".into()],
        output: json!({"id":id,"status":row.status}),
    })
}

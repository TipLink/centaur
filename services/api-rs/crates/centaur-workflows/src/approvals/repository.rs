use super::*;
use sqlx::Row;

pub(crate) struct Repository {
    pub pool: PgPool,
    pub policies: Arc<Policies>,
}

#[derive(sqlx::FromRow)]
pub(crate) struct Record {
    pub id: Uuid,
    pub action: String,
    pub policy_hash: String,
    pub payload: Value,
    pub payload_hash: String,
    pub payload_json: String,
    pub team_id: String,
    pub channel_id: String,
    pub thread_ts: String,
    pub requester_id: String,
    pub status: String,
    pub result: Option<Value>,
    pub message_ts: Option<String>,
    pub message_blocks: Option<Value>,
    pub workflow_task_id: Option<Uuid>,
    pub workflow_run_id: Option<Uuid>,
    pub decided_by: Option<String>,
    pub expires_at: DateTime<Utc>,
}

impl Repository {
    pub async fn context(&self, identity: Identity) -> Result<Value, WorkflowRuntimeError> {
        let ctx = resolve_context(&self.pool, &identity).await?;
        let actions: Vec<_> = self
            .policies
            .iter()
            .filter(|(_, p)| {
                p.team_id == ctx.team_id && p.requester_principals.contains(&identity.principal_id)
            })
            .map(|(name, p)| json!({"action":name,"workflow":p.workflow}))
            .collect();
        Ok(json!({"execution_id":ctx.execution_id,"actions":actions}))
    }

    pub async fn request(
        &self,
        req: Request,
        client: &Client,
    ) -> Result<Value, WorkflowRuntimeError> {
        let identity = Identity {
            sandbox_id: req.sandbox_id,
            principal_id: req.principal_id,
        };
        let ctx = resolve_context(&self.pool, &identity).await?;
        let p = self.policies.get(&req.action).ok_or_else(unavailable)?;
        if ctx.execution_id != req.execution_id
            || p.team_id != ctx.team_id
            || !p.requester_principals.contains(&identity.principal_id)
        {
            return Err(unavailable());
        }
        let (payload, display) = payload(p, req.arguments)?;
        let payload_hash = hash(&payload);
        let policy_hash = hash(p);
        let mut tx = self.pool.begin().await?;
        let active: Option<String> = sqlx::query_scalar("select e.execution_id from session_executions e join sessions s using(thread_key) where e.execution_id=$1 and e.status='running' and s.sandbox_id=$2 and s.iron_control_principal=$3 for update of e,s")
            .bind(&ctx.execution_id).bind(&identity.sandbox_id).bind(&identity.principal_id)
            .fetch_optional(&mut *tx).await?;
        if active.is_none() {
            return Err(unavailable());
        }
        let old = sqlx::query("select id,payload_hash,policy_hash,action from tool_approvals where execution_id=$1 and idempotency_key=$2")
            .bind(&ctx.execution_id).bind(req.idempotency_key).fetch_optional(&mut *tx).await?;
        if let Some(old) = old {
            if old.get::<String, _>("payload_hash") != payload_hash
                || old.get::<String, _>("policy_hash") != policy_hash
                || old.get::<String, _>("action") != req.action
            {
                return Err(WorkflowRuntimeError::BadRequest(
                    "idempotency key already used for different arguments or policy".into(),
                ));
            }
            return Ok(json!({"id":old.get::<Uuid,_>("id")}));
        }
        let count: i64 =
            sqlx::query_scalar("select count(*) from tool_approvals where execution_id=$1")
                .bind(&ctx.execution_id)
                .fetch_one(&mut *tx)
                .await?;
        if count >= 20 {
            return Err(WorkflowRuntimeError::BadRequest(
                "approval request limit reached".into(),
            ));
        }
        let id = Uuid::new_v4();
        let task = client
            .spawn_with_executor(
                WORKFLOW_TASK,
                WorkflowTaskInput {
                    workflow_name: DRIVER_WORKFLOW.into(),
                    input: json!({"approval_id":id}),
                    harness_type: HarnessType::Codex,
                    slack_button_feedback: None,
                },
                SpawnOptions {
                    queue: Some(WORKFLOW_QUEUE.into()),
                    max_attempts: Some(100),
                    idempotency_key: Some(format!("approval:{id}")),
                    ..Default::default()
                },
                &mut *tx,
            )
            .await?;
        sqlx::query("insert into tool_approvals (id,execution_id,thread_key,sandbox_id,principal_id,idempotency_key,action,policy_hash,payload,payload_hash,team_id,channel_id,thread_ts,requester_id,expires_at,payload_json,executor_principal,workflow_task_id,workflow_run_id)
            values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,now()+make_interval(secs=>$15),$16,$17,$18::uuid,$19::uuid)")
            .bind(id).bind(&ctx.execution_id).bind(&ctx.thread_key).bind(&identity.sandbox_id).bind(&identity.principal_id)
            .bind(req.idempotency_key).bind(&req.action).bind(policy_hash).bind(payload).bind(payload_hash)
            .bind(&ctx.team_id).bind(&ctx.channel_id).bind(thread_timestamp(&ctx)?).bind(&ctx.requester_id)
            .bind(f64::from(p.expires_seconds)).bind(display).bind(&p.executor_principal)
            .bind(&task.task_id).bind(&task.run_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(json!({"id":id}))
    }

    pub async fn read(&self, id: Uuid, identity: Identity) -> Result<Value, WorkflowRuntimeError> {
        reconcile_lifecycle(&self.pool, Some(id)).await?;
        sqlx::query_scalar("select jsonb_build_object('id',id,'status',status,'payload_hash',payload_hash,'decided_by',decided_by,'expires_at',expires_at,'result',result,'workflow_task_id',workflow_task_id,'workflow_run_id',workflow_run_id)
            from tool_approvals where id=$1 and sandbox_id=$2 and principal_id=$3")
            .bind(id).bind(identity.sandbox_id).bind(identity.principal_id).fetch_optional(&self.pool)
            .await?.ok_or_else(unavailable)
    }

    pub async fn finish(
        &self,
        id: Uuid,
        policy: &ActionPolicy,
        result: Result<Value, WorkflowRuntimeError>,
    ) -> Result<(), WorkflowRuntimeError> {
        let (status, result) = runner::executor_outcome(policy, result);
        sqlx::query(
            "update tool_approvals set
             status=case when execution_deadline>statement_timestamp() then $2 else 'unknown' end,
             result=case when execution_deadline>statement_timestamp() then $3 else '{\"outcome\":\"unknown\"}'::jsonb end
             where id=$1 and status='executing' and policy_hash=$4",
        )
        .bind(id)
        .bind(status)
        .bind(result)
        .bind(hash(policy))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn cancel(
        &self,
        id: Uuid,
        identity: Identity,
    ) -> Result<Value, WorkflowRuntimeError> {
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("update tool_approvals set status='cancelled' where id=$1 and sandbox_id=$2 and principal_id=$3 and status in ('pending','approved')")
            .bind(id).bind(&identity.sandbox_id).bind(&identity.principal_id).execute(&mut *tx).await?.rows_affected();
        if changed > 0 {
            wake(&self.pool, &mut tx, id).await?;
        }
        tx.commit().await?;
        self.read(id, identity).await
    }

    pub async fn decide(
        &self,
        button: slack_buttons::VerifiedButton,
    ) -> Result<CreateWorkflowRunResponse, WorkflowRuntimeError> {
        if button.request.workflow_name != DECISION_WORKFLOW {
            return Err(unavailable());
        }
        let input = button.request.input;
        let id = input["approval_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(unavailable)?;
        reconcile_lifecycle(&self.pool, Some(id)).await?;
        let mut tx = self.pool.begin().await?;
        let row =
            sqlx::query_as::<_, Record>("select * from tool_approvals where id=$1 for update")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(unavailable)?;
        let p = self.policies.get(&row.action).ok_or_else(unavailable)?;
        validate_decision(p, &row, &input, button.message.as_ref())?;
        let user = input["click"]["user_id"].as_str().ok_or_else(unavailable)?;
        let decision = input["click"]["action"].as_str().ok_or_else(unavailable)?;
        let changed = sqlx::query("update tool_approvals set status=$2,decided_by=$3,decided_at=clock_timestamp() where id=$1 and status='pending' and expires_at>clock_timestamp()")
            .bind(id).bind(decision).bind(user).execute(&mut *tx).await?.rows_affected();
        if changed == 0 && !(row.status == decision && row.decided_by.as_deref() == Some(user)) {
            return Err(rejected(match row.status.as_str() {
                "pending" | "expired" => DecisionRejection::Expired,
                "approved" => DecisionRejection::AlreadyApproved,
                "declined" => DecisionRejection::AlreadyDeclined,
                "cancelled" => DecisionRejection::Cancelled,
                "executing" => DecisionRejection::Executing,
                "succeeded" => DecisionRejection::Succeeded,
                "unknown" => DecisionRejection::Unknown,
                _ => return Err(unavailable()),
            }));
        }
        if changed > 0 {
            wake(&self.pool, &mut tx, id).await?;
        }
        tx.commit().await?;
        Ok(CreateWorkflowRunResponse {
            ok: true,
            run_id: row.workflow_run_id.ok_or_else(unavailable)?.to_string(),
            initial_run_id: row.workflow_run_id.ok_or_else(unavailable)?.to_string(),
            task_id: row.workflow_task_id.ok_or_else(unavailable)?.to_string(),
            status: decision.into(),
            created: changed > 0,
        })
    }

    pub async fn load(&self, id: Uuid, task_id: &str) -> Result<Record, WorkflowRuntimeError> {
        sqlx::query_as("select * from tool_approvals where id=$1 and workflow_task_id=$2::uuid")
            .bind(id)
            .bind(task_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(unavailable)
    }

    pub async fn refresh(
        &self,
        id: Uuid,
        policy: Option<&ActionPolicy>,
    ) -> Result<(), WorkflowRuntimeError> {
        reconcile_lifecycle(&self.pool, Some(id)).await?;
        sqlx::query("update tool_approvals set status=case when expires_at<=now() then 'expired' else 'cancelled' end where id=$1 and status in ('pending','approved') and (expires_at<=now() or policy_hash<>$2)")
            .bind(id).bind(policy.map(hash).unwrap_or_default()).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn claim(
        &self,
        id: Uuid,
        policy: &ActionPolicy,
    ) -> Result<Option<super::execution::Deadline>, WorkflowRuntimeError> {
        let mut tx = self.pool.begin().await?;
        // Serialize with decisions/cancellation before locking the native task.
        // Native cancellation commits its run/task changes before reconciling
        // the approval, avoiding an inverted application/native lock order.
        sqlx::query("select id from tool_approvals where id=$1 for update")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        // A cancelled/exhausted task must not win a late execution claim.
        let state: Option<String> = sqlx::query_scalar(
            "select t.state from absurd.t_centaur_workflows t join tool_approvals a on a.workflow_task_id=t.task_id where a.id=$1 for update of t"
        ).bind(id).fetch_optional(&mut *tx).await?;
        reconcile_lifecycle(&mut *tx, Some(id)).await?;
        // Anchor before the query, not after its response or commit. The server's
        // remaining budget is therefore conservative even with latency or clock skew.
        let started = tokio::time::Instant::now();
        let remaining: Option<f64> = if matches!(
            state.as_deref(),
            Some("pending" | "running" | "sleeping")
        ) {
            sqlx::query_scalar("update tool_approvals set status='executing',execution_deadline=clock_timestamp()+make_interval(secs=>$2) where id=$1 and status='approved' and expires_at>clock_timestamp() and policy_hash=$3 returning greatest(0,extract(epoch from execution_deadline-clock_timestamp()))::double precision")
                .bind(id).bind(f64::from(policy.timeout_seconds)).bind(hash(policy)).fetch_optional(&mut *tx).await?
        } else {
            None
        };
        tx.commit().await?;
        Ok(remaining.map(|seconds| super::execution::Deadline::from_remaining(started, seconds)))
    }

    pub async fn recover(&self, id: Uuid) -> Result<(), WorkflowRuntimeError> {
        sqlx::query(
            "update tool_approvals set status='unknown' where id=$1 and status='executing'",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn validate_decision(
    p: &ActionPolicy,
    row: &Record,
    input: &Value,
    message: Option<&Value>,
) -> Result<(), WorkflowRuntimeError> {
    let click = &input["click"];
    let user = click["user_id"].as_str().unwrap_or_default();
    if p.team_id != row.team_id
        || click["team_id"] != row.team_id
        || !matches!(click["action"].as_str(), Some("approved" | "declined"))
        || click["id"] != row.id.to_string()
        || click["channel_id"] != row.channel_id
        || row.message_ts.as_deref() != click["message_ts"].as_str()
        || row.message_ts.is_none()
        || input["payload_hash"] != row.payload_hash
        || row.message_blocks.as_ref().is_none_or(|blocks| {
            message.is_none_or(|m| {
                !m["blocks"].is_array() || visible_blocks(blocks) != visible_blocks(&m["blocks"])
            })
        })
    {
        return Err(unavailable());
    }
    // Reveal actionable reasons only after the original card/message binding
    // has passed. Non-approvers learn only that they cannot decide this action.
    if !p.approver_user_ids.iter().any(|s| s == user) {
        return Err(rejected(DecisionRejection::NotApprover));
    }
    if hash(p) != row.policy_hash {
        return Err(rejected(DecisionRejection::PolicyChanged));
    }
    if !p.allow_self_approval && row.requester_id == user {
        return Err(rejected(DecisionRejection::SelfApprovalNotAllowed));
    }
    Ok(())
}

async fn wake(
    pool: &PgPool,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
) -> Result<(), WorkflowRuntimeError> {
    // The SDK owns queue SQL; the application owns the decision transaction.
    let client = Client::from_pool_with_options(
        pool.clone(),
        ClientOptions {
            queue_name: WORKFLOW_QUEUE.into(),
            ..Default::default()
        },
    )?;
    client
        .emit_event_with_executor(&format!("{EVENT_PREFIX}{id}"), json!({}), None, &mut **tx)
        .await?;
    Ok(())
}

/// Reconcile from durable task state, never from an in-process completion hook.
/// Preserve known outcomes; uncertainty can never restore execution authority.
pub(crate) async fn reconcile_lifecycle<'e, E>(
    executor: E,
    id: Option<Uuid>,
) -> Result<(), WorkflowRuntimeError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query("update tool_approvals a set
        status=case when a.status='executing' then 'unknown' else 'cancelled' end,
        result=case when a.status='executing' then '{\"outcome\":\"unknown\"}'::jsonb else null end
        where a.workflow_task_id is not null and ($1::uuid is null or a.id=$1)
        and a.status in ('pending','approved','executing')
        and (not exists(select 1 from absurd.t_centaur_workflows t
                        where t.task_id=a.workflow_task_id and t.state in ('pending','running','sleeping'))
             or (a.status='executing' and a.execution_deadline<=now()))")
        .bind(id).execute(executor)
        .await?;
    Ok(())
}

async fn resolve_context(
    pool: &PgPool,
    identity: &Identity,
) -> Result<Context, WorkflowRuntimeError> {
    let mut rows = sqlx::query_as::<_, Context>(
        "select e.execution_id, s.thread_key, e.metadata->>'slack_team_id' as team_id,
         e.metadata->>'slack_channel_id' as channel_id, e.metadata->>'slack_user_id' as requester_id
         from sessions s join session_executions e using(thread_key)
         where s.sandbox_id=$1 and s.iron_control_principal=$2 and e.status='running'
         and e.metadata->>'source'='slackbotv2' and e.metadata->>'platform'='slack'
         and e.metadata->>'slack_home_team_id'=e.metadata->>'slack_team_id'
         and e.metadata->>'slack_user_id' is not null and e.metadata->>'slack_channel_id' is not null")
        .bind(&identity.sandbox_id).bind(&identity.principal_id).fetch_all(pool).await?;
    if rows.len() != 1 {
        return Err(unavailable());
    }
    let ctx = rows.remove(0);
    thread_timestamp(&ctx)?;
    Ok(ctx)
}

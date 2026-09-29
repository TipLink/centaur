//! PostgreSQL repository for approval admission, decisions, delivery and claims.
//! Locks and compare-and-set transitions are the durable coordination boundary.
use super::*;

pub(super) struct ApprovalRepository {
    pool: PgPool,
    policies: Arc<Policies>,
}

impl ApprovalRepository {
    pub(super) fn from_state(state: &AppState) -> Result<Self, ApiError> {
        Ok(Self {
            pool: state.pool()?,
            policies: state.approval_policies(),
        })
    }

    pub(super) async fn context(&self, identity: Identity) -> Result<Json<Value>, ApiError> {
        let ctx = resolve_context(&self.pool.clone(), &identity).await?;
        let actions: Vec<_> = self
            .policies
            .clone()
            .iter()
            .filter(|(_, p)| {
                p.team_id == ctx.team_id && p.requester_principals.contains(&identity.principal_id)
            })
            .map(|(name, p)| json!({"action":name,"tool":p.tool,"method":p.method}))
            .collect();
        Ok(Json(
            json!({"execution_id":ctx.execution_id,"actions":actions}),
        ))
    }

    pub(super) async fn request(&self, req: Request) -> Result<Json<Value>, ApiError> {
        let pool = self.pool.clone();
        let identity = Identity {
            sandbox_id: req.sandbox_id,
            principal_id: req.principal_id,
        };
        let ctx = resolve_context(&pool, &identity).await?;
        if ctx.execution_id != req.execution_id {
            return Err(unavailable());
        }
        let policies = self.policies.clone();
        let p = policies.get(&req.action).ok_or_else(unavailable)?;
        if p.team_id != ctx.team_id || !p.requester_principals.contains(&identity.principal_id) {
            return Err(unavailable());
        }
        let payload = payload(p, req.arguments)?;
        let payload_hash = hash(&payload);
        let payload_json = display_payload(&payload)?;
        let policy_hash = hash(p);
        let mut tx = pool.begin().await?;
        // Serialize admission against cancellation and bound spam per execution.
        let active: Option<String> = sqlx::query_scalar("select execution_id from session_executions where execution_id=$1 and status='running' for update")
        .bind(&ctx.execution_id).fetch_optional(&mut *tx).await?;
        if active.is_none() {
            return Err(unavailable());
        }
        let old = sqlx::query("select id, payload_hash, policy_hash, action from tool_approvals where execution_id=$1 and idempotency_key=$2")
        .bind(&ctx.execution_id).bind(req.idempotency_key).fetch_optional(&mut *tx).await?;
        if let Some(old) = old {
            if old.get::<String, _>("payload_hash") != payload_hash
                || old.get::<String, _>("policy_hash") != policy_hash
                || old.get::<String, _>("action") != req.action
            {
                return Err(ApiError::BadRequest(
                    "idempotency key was already used for different arguments or policy".into(),
                ));
            }
            return Ok(Json(json!({"id":old.get::<Uuid,_>("id")})));
        }
        let count: i64 =
            sqlx::query_scalar("select count(*) from tool_approvals where execution_id=$1")
                .bind(&ctx.execution_id)
                .fetch_one(&mut *tx)
                .await?;
        if count >= 20 {
            return Err(ApiError::BadRequest(
                "approval request limit reached for execution".into(),
            ));
        }
        let id = Uuid::new_v4();
        sqlx::query("insert into tool_approvals (id,execution_id,thread_key,sandbox_id,principal_id,idempotency_key,action,policy_hash,payload,payload_hash,team_id,channel_id,thread_ts,requester_id,expires_at,payload_json,executor_principal)
        values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,now()+make_interval(secs=>$15),$16,$17)")
        .bind(id).bind(&ctx.execution_id).bind(&ctx.thread_key).bind(&identity.sandbox_id).bind(&identity.principal_id)
        .bind(req.idempotency_key).bind(&req.action).bind(policy_hash).bind(payload).bind(payload_hash)
        .bind(&ctx.team_id).bind(&ctx.channel_id).bind(thread_timestamp(&ctx)?).bind(&ctx.requester_id)
        .bind(f64::from(p.expires_seconds)).bind(payload_json).bind(&p.executor_principal).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Json(json!({"id":id})))
    }

    pub(super) async fn read(&self, id: Uuid, identity: Identity) -> Result<Json<Value>, ApiError> {
        let row: Option<Value> = sqlx::query_scalar("select jsonb_build_object('id',id,'status',status,'payload_hash',payload_hash,'decided_by',decided_by,'expires_at',expires_at,'result',result)
        from tool_approvals where id=$1 and sandbox_id=$2 and principal_id=$3")
        .bind(id).bind(identity.sandbox_id).bind(identity.principal_id).fetch_optional(&self.pool.clone()).await?;
        Ok(Json(row.ok_or_else(unavailable)?))
    }

    pub(super) async fn decide(&self, id: Uuid, click: Decision) -> Result<Json<Value>, ApiError> {
        let pool = self.pool.clone();
        let mut tx = pool.begin().await?;
        let row = sqlx::query("select * from tool_approvals where id=$1 for update")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(unavailable)?;
        let policies = self.policies.clone();
        let p = policies
            .get(&row.get::<String, _>("action"))
            .ok_or_else(unavailable)?;
        if !allowed_decision(
            p,
            &row.get::<String, _>("policy_hash"),
            &row.get::<String, _>("team_id"),
            &row.get::<String, _>("requester_id"),
            &click,
        ) || row.get::<String, _>("channel_id") != click.channel_id
            || row.get::<Option<String>, _>("message_ts").as_deref() != Some(&click.message_ts)
            || row.get::<String, _>("payload_hash") != click.payload_hash
            || !click.message_blocks.is_array()
            || row
                .get::<Option<Value>, _>("message_blocks")
                .is_none_or(|blocks| {
                    visible_blocks(&blocks) != visible_blocks(&click.message_blocks)
                })
        {
            return Err(unavailable());
        }
        let changed = sqlx::query("update tool_approvals a set status=$2,decided_by=$3,decided_at=now(),revision=revision+1
        where id=$1 and status='pending' and expires_at>now()
        and exists(select 1 from session_executions e join sessions s using(thread_key)
          where e.execution_id=a.execution_id and e.status='running' and s.sandbox_id=a.sandbox_id and s.iron_control_principal=a.principal_id)")
        .bind(id).bind(&click.decision).bind(&click.user_id).execute(&mut *tx).await?.rows_affected();
        if changed == 0
            && !(row.get::<String, _>("status") == click.decision
                && row.get::<Option<String>, _>("decided_by").as_deref() == Some(&click.user_id))
        {
            return Err(unavailable());
        }
        tx.commit().await?;
        Ok(Json(json!({"outcome":"accepted"})))
    }

    pub(super) async fn claim_delivery(&self, req: DeliveryClaim) -> Result<Json<Value>, ApiError> {
        let token = Uuid::new_v4();
        let row: Option<Value> = sqlx::query_scalar("update tool_approvals a set delivery_token=$2,delivery_until=now()+interval '30 seconds'
        where id=(select id from tool_approvals where team_id=$1 and delivered_revision<revision
          and (delivery_until is null or delivery_until<now()) order by created_at for update skip locked limit 1)
        returning jsonb_build_object('id',id,'token',delivery_token,'revision',revision,'payload_json',payload_json,'payload_hash',payload_hash,
          'action',action,'status',status,'team_id',team_id,'channel_id',channel_id,'thread_ts',thread_ts,
          'requester_id',requester_id,'decided_by',decided_by,'message_ts',message_ts,'expires_at',expires_at)")
        .bind(req.team_id).bind(token).fetch_optional(&self.pool.clone()).await?;
        Ok(Json(json!({"delivery":row})))
    }

    pub(super) async fn delivered(
        &self,
        id: Uuid,
        req: Delivered,
    ) -> Result<Json<Value>, ApiError> {
        if !timestamp(&req.message_ts) || req.revision < 1 || !req.message_blocks.is_array() {
            return Err(unavailable());
        }
        let changed = sqlx::query("update tool_approvals set message_ts=coalesce(message_ts,$3),message_blocks=$5,delivered_revision=$4,delivery_until=null,delivery_token=null
        where id=$1 and delivery_token=$2 and $4<=revision and $4>=delivered_revision and (message_ts is null or message_ts=$3)")
        .bind(id).bind(req.token).bind(req.message_ts).bind(req.revision).bind(req.message_blocks).execute(&self.pool.clone()).await?.rows_affected();
        if changed == 0 {
            return Err(unavailable());
        }
        Ok(Json(json!({"ok":true})))
    }

    pub(super) async fn claim_approved(
        &self,
    ) -> Result<Vec<(Uuid, ActionPolicy, Value)>, ApiError> {
        let pool = self.pool.clone();
        sqlx::query("update tool_approvals a set status=case when expires_at<=now() then 'expired' else 'cancelled' end, revision=revision+1
        where status in ('pending','approved') and (expires_at<=now() or not exists
        (select 1 from session_executions e join sessions s using(thread_key) where e.execution_id=a.execution_id
         and e.status='running' and s.sandbox_id=a.sandbox_id and s.iron_control_principal=a.principal_id))")
        .execute(&pool).await?;
        sqlx::query("update tool_approvals set status='unknown',revision=revision+1 where status='executing' and execution_deadline<now()")
        .execute(&pool).await?;
        let policies = self.policies.clone();
        let hashes: BTreeMap<_, _> = policies
            .iter()
            .map(|(action, p)| (action.clone(), hash(p)))
            .collect();
        sqlx::query("update tool_approvals set status='cancelled',revision=revision+1 where status in ('pending','approved') and policy_hash<>coalesce($1->>action,'')")
        .bind(json!(hashes)).execute(&pool).await?;
        // A row in executing is never reclaimed, even on timeout.
        let rows = sqlx::query("select id,action,policy_hash from tool_approvals a where status='approved'
            and not exists(select 1 from tool_approvals busy where busy.executor_principal=a.executor_principal and busy.status='executing')
            order by created_at limit 1")
        .fetch_all(&pool).await?;
        let mut claimed = Vec::new();
        for row in rows {
            let id: Uuid = row.get("id");
            let p = policies.get(&row.get::<String, _>("action"));
            if p.is_none_or(|p| hash(p) != row.get::<String, _>("policy_hash")) {
                sqlx::query("update tool_approvals set status='cancelled',revision=revision+1 where id=$1 and status in ('pending','approved')")
                .bind(id).execute(&pool).await?;
                continue;
            }
            let p = p.expect("checked").clone();
            // Lock the source execution before committing the execution authorization.
            let mut tx = pool.begin().await?;
            // Serialize claims for this executor across API replicas. Other
            // approved requests remain queued, with their original expiry.
            let executor_lock: bool = sqlx::query_scalar(
                "select pg_try_advisory_xact_lock(hashtextextended('tool-approval:' || $1,0))",
            )
            .bind(&p.executor_principal)
            .fetch_one(&mut *tx)
            .await?;
            if !executor_lock {
                continue;
            }
            let busy: bool = sqlx::query_scalar("select exists(select 1 from tool_approvals where executor_principal=$1 and status='executing')")
                .bind(&p.executor_principal).fetch_one(&mut *tx).await?;
            if busy {
                continue;
            }
            let active: Option<String> = sqlx::query_scalar("select e.execution_id from session_executions e join tool_approvals a using(execution_id)
            join sessions s on s.thread_key=e.thread_key where a.id=$1 and e.status='running'
            and s.sandbox_id=a.sandbox_id and s.iron_control_principal=a.principal_id for update of e,s")
            .bind(id).fetch_optional(&mut *tx).await?;
            if active.is_none() {
                continue;
            }
            let payload: Option<Value> = sqlx::query_scalar("update tool_approvals set status='executing',revision=revision+1,
            execution_deadline=now()+make_interval(secs=>$2) where id=$1 and status='approved' and expires_at>now() returning payload")
            .bind(id).bind(f64::from(p.timeout_seconds + 120)).fetch_optional(&mut *tx).await?;
            tx.commit().await?;
            if let Some(payload) = payload {
                claimed.push((id, p, payload));
            }
        }
        Ok(claimed)
    }
}

async fn resolve_context(pool: &PgPool, identity: &Identity) -> Result<Context, ApiError> {
    let rows = sqlx::query_as::<_, Context>(
        "select e.execution_id, s.thread_key, e.metadata->>'slack_team_id' as team_id,
         e.metadata->>'slack_channel_id' as channel_id, e.metadata->>'slack_user_id' as requester_id
         from sessions s join session_executions e using(thread_key)
         where s.sandbox_id=$1 and s.iron_control_principal=$2 and e.status='running'
         and e.metadata->>'source'='slackbotv2' and e.metadata->>'platform'='slack'
         and e.metadata->>'slack_home_team_id'=e.metadata->>'slack_team_id'
         and e.metadata->>'slack_user_id' is not null and e.metadata->>'slack_channel_id' is not null")
        .bind(&identity.sandbox_id).bind(&identity.principal_id).fetch_all(pool).await?;
    let mut rows = rows;
    if rows.len() != 1 {
        return Err(unavailable());
    }
    let ctx = rows.remove(0);
    thread_timestamp(&ctx)?;
    Ok(ctx)
}

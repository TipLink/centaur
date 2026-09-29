//! One-shot, durable authorization of a configured tool method. Only Console
//! attests sandbox identity; only the verified Slack ingress attests clicks.
use std::{collections::BTreeMap, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};
use centaur_session_runtime::{ToolHostCallInput, ToolHostInvocation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::{ApiError, AppState};

mod repository;
use repository::ApprovalRepository;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionPolicy {
    pub tool: String,
    pub method: String,
    pub executor_principal: String,
    pub requester_principals: Vec<String>,
    pub team_id: String,
    pub approver_user_ids: Vec<String>,
    pub allow_self_approval: bool,
    pub expires_seconds: u32,
    pub timeout_seconds: u32,
    /// Review identifier for a pinned tool implementation, not an agent value.
    pub implementation_revision: String,
}

pub type Policies = BTreeMap<String, ActionPolicy>;

pub fn parse_policies(raw: &str) -> Result<Policies, String> {
    let policies: Policies =
        serde_json::from_str(raw).map_err(|_| "invalid approval policy JSON")?;
    for (name, p) in &policies {
        if !identifier(name)
            || !identifier(&p.tool)
            || !identifier(&p.method)
            || p.method.starts_with('_')
            || !p.executor_principal.starts_with("prn_")
            || p.requester_principals.is_empty()
            || p.approver_user_ids.is_empty()
            || p.requester_principals
                .iter()
                .any(|s| !s.starts_with("prn_") || s == &p.executor_principal)
            || !slack_id(&p.team_id, 'T')
            || p.approver_user_ids
                .iter()
                .any(|s| !slack_id(s, 'U') && !slack_id(s, 'W'))
            || !(30..=900).contains(&p.expires_seconds)
            || !(1..=300).contains(&p.timeout_seconds)
            || p.implementation_revision.trim().is_empty()
        {
            return Err(format!("invalid approval policy for {name}"));
        }
    }
    // An executor must never be an ordinary requester for another action.
    if policies.values().any(|p| {
        policies
            .values()
            .any(|q| q.requester_principals.contains(&p.executor_principal))
    }) {
        return Err("approval executor is also a requester".into());
    }
    Ok(policies)
}

fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}
fn slack_id(s: &str, prefix: char) -> bool {
    s.starts_with(prefix)
        && (2..=32).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}
fn timestamp(s: &str) -> bool {
    let Some((a, b)) = s.split_once('.') else {
        return false;
    };
    !a.is_empty()
        && !b.is_empty()
        && s.len() <= 32
        && a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit())
}
fn hash(value: &impl Serialize) -> String {
    // serde_json uses sorted map keys (preserve_order is not enabled).
    hex::encode(Sha256::digest(
        serde_json::to_vec(value).expect("JSON serializable"),
    ))
}
fn unavailable() -> ApiError {
    ApiError::Forbidden("tool approval is unavailable".into())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/tool-approvals/context", post(context))
        .route("/api/tool-approvals/request", post(request))
        .route("/api/tool-approvals/{id}/read", post(read))
        .route("/api/tool-approvals/{id}/decide", post(decide))
        .route("/api/tool-approvals/delivery/claim", post(claim_delivery))
        .route("/api/tool-approvals/{id}/delivered", post(delivered))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    sandbox_id: String,
    principal_id: String,
}

#[derive(sqlx::FromRow, Serialize)]
struct Context {
    execution_id: String,
    thread_key: String,
    team_id: String,
    channel_id: String,
    requester_id: String,
}

fn thread_timestamp(ctx: &Context) -> Result<String, ApiError> {
    // Use the durable canonical thread; never accept a destination from the tool.
    let prefix = format!("slack:{}:", ctx.channel_id);
    let qualified = format!("slack:{}:{}:", ctx.team_id, ctx.channel_id);
    let ts = ctx
        .thread_key
        .strip_prefix(&qualified)
        .or_else(|| ctx.thread_key.strip_prefix(&prefix))
        .ok_or_else(unavailable)?;
    if !timestamp(ts) {
        return Err(unavailable());
    }
    Ok(ts.to_owned())
}

async fn context(
    State(state): State<AppState>,
    Json(identity): Json<Identity>,
) -> Result<Json<Value>, ApiError> {
    ApprovalRepository::from_state(&state)?
        .context(identity)
        .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    sandbox_id: String,
    principal_id: String,
    execution_id: String,
    idempotency_key: Uuid,
    action: String,
    arguments: Value,
}

fn payload(policy: &ActionPolicy, arguments: Value) -> Result<Value, ApiError> {
    if !arguments.is_object() {
        return Err(ApiError::BadRequest(
            "arguments must be a JSON object".into(),
        ));
    }
    let payload = json!({"tool":policy.tool,"method":policy.method,"arguments":arguments,"executor_principal":policy.executor_principal,
        "implementation_revision":policy.implementation_revision});
    // The entire payload fits in plaintext Slack sections. No truncation, files,
    // deferred reads or presigned URLs are added by the gateway.
    if display_payload(&payload)?.len() > 12000 {
        return Err(ApiError::PayloadTooLarge(
            "approval payload exceeds 12000 bytes".into(),
        ));
    }
    Ok(payload)
}

fn display_payload(payload: &Value) -> Result<String, serde_json::Error> {
    let mut text = String::new();
    for ch in serde_json::to_string_pretty(payload)?.chars() {
        // JSON escapes preserve the value while making bidi controls, invisible
        // Unicode and Slack entity/mention delimiters unambiguous to reviewers.
        if !ch.is_ascii() || matches!(ch, '<' | '>' | '&') {
            for unit in ch.encode_utf16(&mut [0; 2]) {
                text.push_str(&format!("\\u{unit:04x}"));
            }
        } else {
            text.push(ch);
        }
    }
    Ok(text)
}

async fn request(
    State(state): State<AppState>,
    Json(req): Json<Request>,
) -> Result<Json<Value>, ApiError> {
    ApprovalRepository::from_state(&state)?.request(req).await
}

async fn read(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(identity): Json<Identity>,
) -> Result<Json<Value>, ApiError> {
    ApprovalRepository::from_state(&state)?
        .read(id, identity)
        .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    team_id: String,
    channel_id: String,
    message_ts: String,
    user_id: String,
    payload_hash: String,
    decision: String,
    message_blocks: Value,
}

fn visible_blocks(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(visible_blocks).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                // Slack adds generated block IDs and default emoji rendering flags.
                .filter(|(key, value)| {
                    key.as_str() != "block_id"
                        && !(key.as_str() == "emoji" && **value == Value::Bool(true))
                })
                .map(|(key, value)| (key.clone(), visible_blocks(value)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn allowed_decision(
    p: &ActionPolicy,
    stored_policy_hash: &str,
    team: &str,
    requester: &str,
    click: &Decision,
) -> bool {
    hash(p) == stored_policy_hash
        && p.team_id == team
        && click.team_id == team
        && p.approver_user_ids.contains(&click.user_id)
        && (p.allow_self_approval || requester != click.user_id)
        && matches!(click.decision.as_str(), "approved" | "declined")
}

async fn decide(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(click): Json<Decision>,
) -> Result<Json<Value>, ApiError> {
    ApprovalRepository::from_state(&state)?
        .decide(id, click)
        .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliveryClaim {
    team_id: String,
}

async fn claim_delivery(
    State(state): State<AppState>,
    Json(req): Json<DeliveryClaim>,
) -> Result<Json<Value>, ApiError> {
    ApprovalRepository::from_state(&state)?
        .claim_delivery(req)
        .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delivered {
    token: Uuid,
    revision: i64,
    message_ts: String,
    message_blocks: Value,
}

async fn delivered(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<Delivered>,
) -> Result<Json<Value>, ApiError> {
    ApprovalRepository::from_state(&state)?
        .delivered(id, req)
        .await
}

/// Run once per process. Claims are durable across replicas and restarts.
pub fn spawn_worker(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            if let Err(error) = tick(&state).await {
                // SQL errors can include input values; do not log their contents.
                let _ = error;
                tracing::warn!("tool approval worker iteration failed");
            }
        }
    });
}

async fn tick(state: &AppState) -> Result<(), ApiError> {
    for (id, policy, payload) in claim_approved(state).await? {
        let state = state.clone();
        tokio::spawn(async move {
            execute(state, id, policy, payload).await;
        });
    }
    Ok(())
}

async fn claim_approved(state: &AppState) -> Result<Vec<(Uuid, ActionPolicy, Value)>, ApiError> {
    ApprovalRepository::from_state(state)?
        .claim_approved()
        .await
}

async fn execute(state: AppState, id: Uuid, p: ActionPolicy, payload: Value) {
    let run = async {
        let runtime = state.runtime()?;
        let policy = runtime
            .resolve_tool_host_call_policy(&p.executor_principal)
            .await?;
        let output = runtime
            .run_tool_host_call(
                ToolHostCallInput {
                    idempotency_key: Some(format!("approval-call-{id}")),
                    principal_id: p.executor_principal,
                    console_user_email: None,
                    console_user_name: None,
                    token_id: Some(format!("approval:{id}")),
                    tool_name: p.tool,
                    invocation: ToolHostInvocation::V1 {
                        method: p.method,
                        arguments: payload["arguments"].clone(),
                    },
                    timeout: Duration::from_secs(u64::from(p.timeout_seconds)),
                },
                policy,
            )
            .await
            .map_err(|_| ApiError::Internal("approved tool outcome uncertain".into()))?;
        let status = if output.timed_out {
            "unknown"
        } else if output.exit_status == Some(0) {
            "succeeded"
        } else {
            "failed"
        };
        // Tools must have reviewed output redaction. Bound stored output, never log it.
        let truncate = |s: String| s.chars().take(32000).collect::<String>();
        Ok::<_, ApiError>((
            status,
            json!({"stdout":truncate(output.stdout),"stderr":truncate(output.stderr),
            "exit_status":output.exit_status,"timed_out":output.timed_out,"execution_id":output.execution_id}),
        ))
    };
    let (status, result) = match tokio::time::timeout(
        Duration::from_secs(u64::from(p.timeout_seconds) + 90),
        run,
    )
    .await
    {
        Ok(Ok(result)) => result,
        _ => (
            "unknown",
            json!({"error":"Outcome uncertain; inspect the provider before submitting another request."}),
        ),
    };
    if let Ok(pool) = state.pool() {
        let _ = sqlx::query("update tool_approvals set status=$2,result=$3,revision=revision+1 where id=$1 and status='executing'")
            .bind(id).bind(status).bind(result).execute(&pool).await;
    }
}

pub fn policies_from_env() -> Result<Arc<Policies>, String> {
    parse_policies(&std::env::var("CENTAUR_TOOL_APPROVAL_POLICIES").unwrap_or_else(|_| "{}".into()))
        .map(Arc::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> ActionPolicy {
        ActionPolicy {
            tool: "example".into(),
            method: "create".into(),
            executor_principal: "prn_executor".into(),
            requester_principals: vec!["prn_source".into()],
            team_id: "TEXAMPLE".into(),
            approver_user_ids: vec!["UALICE".into()],
            allow_self_approval: false,
            expires_seconds: 600,
            timeout_seconds: 120,
            implementation_revision: "reviewed-revision".into(),
        }
    }
    #[test]
    fn rejects_unconfigured_and_overlapping_authority() {
        assert!(parse_policies("{}").unwrap().is_empty());
        let mut p = policy();
        p.requester_principals.push(p.executor_principal.clone());
        assert!(parse_policies(&json!({"create":p}).to_string()).is_err());
        p = policy();
        p.approver_user_ids.clear();
        assert!(parse_policies(&json!({"create":p}).to_string()).is_err());
    }

    #[test]
    fn destination_and_card_validation_reject_substitution() {
        let mut ctx = Context {
            execution_id: "exe".into(),
            thread_key: "slack:TEXAMPLE:CTEST:1.000".into(),
            team_id: "TEXAMPLE".into(),
            channel_id: "CTEST".into(),
            requester_id: "UALICE".into(),
        };
        assert_eq!(thread_timestamp(&ctx).unwrap(), "1.000");
        ctx.thread_key = "slack:CTEST:1.000".into();
        assert_eq!(thread_timestamp(&ctx).unwrap(), "1.000");
        ctx.thread_key = "slack:TOTHER:CTEST:1.000".into();
        assert!(thread_timestamp(&ctx).is_err());
        let blocks =
            json!([{"type":"section","text":{"type":"plain_text","text":"payload","emoji":false}}]);
        let mut changed = blocks.clone();
        changed[0]["block_id"] = json!("server-generated");
        assert_eq!(visible_blocks(&blocks), visible_blocks(&changed));
        changed[0]["text"]["emoji"] = json!(true);
        assert_ne!(visible_blocks(&blocks), visible_blocks(&changed));
    }
    #[test]
    fn approval_binds_current_policy_workspace_and_actor() {
        let p = policy();
        let digest = hash(&p);
        let mut click = Decision {
            team_id: "TEXAMPLE".into(),
            channel_id: "C1".into(),
            message_ts: "1.1".into(),
            user_id: "UALICE".into(),
            payload_hash: "hash".into(),
            decision: "approved".into(),
            message_blocks: json!([]),
        };
        assert!(allowed_decision(&p, &digest, "TEXAMPLE", "UBOB", &click));
        assert!(!allowed_decision(&p, &digest, "TEXAMPLE", "UALICE", &click));
        click.user_id = "UMALLORY".into();
        assert!(!allowed_decision(&p, &digest, "TEXAMPLE", "UBOB", &click));
        click.user_id = "UALICE".into();
        click.team_id = "TOTHER".into();
        assert!(!allowed_decision(&p, &digest, "TEXAMPLE", "UBOB", &click));
        click.team_id = "TEXAMPLE".into();
        assert!(!allowed_decision(
            &p,
            "old-policy",
            "TEXAMPLE",
            "UBOB",
            &click
        ));
    }
    #[test]
    fn payload_is_complete_bounded_and_order_independent() {
        assert_eq!(hash(&json!({"a":1,"b":2})), hash(&json!({"b":2,"a":1})));
        assert!(payload(&policy(), json!({"body":"x".repeat(12000)})).is_err());
        assert!(payload(&policy(), json!([])).is_err());
        assert_eq!(
            payload(&policy(), json!({"body":"hi"})).unwrap()["arguments"]["body"],
            "hi"
        );
        let value = json!({"body":"\u{202e}<!channel>😀","large":9007199254740993_u64});
        let text = display_payload(&value).unwrap();
        assert!(text.is_ascii());
        assert!(!text.contains('<'));
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), value);
    }

    #[tokio::test]
    async fn postgres_approval_lifecycle_and_races() {
        let Ok(url) = std::env::var("TOOL_APPROVAL_TEST_DATABASE_URL") else {
            eprintln!("skipping: TOOL_APPROVAL_TEST_DATABASE_URL not set");
            return;
        };
        let admin = PgPool::connect(&url).await.unwrap();
        let schema = format!("approval_test_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("create schema {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let search = format!("set search_path to {schema}");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .after_connect(move |conn, _| {
                let search = search.clone();
                Box::pin(async move {
                    sqlx::query(&search).execute(conn).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        // Exercise the migration against the actual session key/foreign-key schema,
        // without requiring unrelated search/vector extensions in this test DB.
        sqlx::raw_sql(include_str!(
            "../../centaur-session-sqlx/migrations/0001_session_control_plane.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../centaur-session-sqlx/migrations/0003_session_iron_control_principal.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../centaur-session-sqlx/migrations/0055_tool_approvals.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("insert into sessions(thread_key,sandbox_id,harness_type,status,iron_control_principal) values('slack:CTEST:1.000','sandbox-test','codex','active','prn_source')")
            .execute(&pool).await.unwrap();
        sqlx::query("insert into session_executions(execution_id,thread_key,status,metadata) values ('exe_test','slack:CTEST:1.000','running',$1)")
            .bind(json!({"source":"slackbotv2","platform":"slack","slack_team_id":"TEXAMPLE","slack_home_team_id":"TEXAMPLE","slack_channel_id":"CTEST","slack_user_id":"UBOB"})).execute(&pool).await.unwrap();
        let mut p = policy();
        p.approver_user_ids.push("UCAROL".into());
        let state = crate::tests::approval_test_state(pool.clone())
            .with_approval_policies(BTreeMap::from([("create".into(), p)]));
        let req = |key: Uuid, arguments: Value| Request {
            sandbox_id: "sandbox-test".into(),
            principal_id: "prn_source".into(),
            execution_id: "exe_test".into(),
            idempotency_key: key,
            action: "create".into(),
            arguments,
        };
        let identity = || Identity {
            sandbox_id: "sandbox-test".into(),
            principal_id: "prn_source".into(),
        };
        let key = Uuid::new_v4();
        let id: Uuid = serde_json::from_value(
            request(
                State(state.clone()),
                Json(req(key, json!({"body":"frozen"}))),
            )
            .await
            .unwrap()
            .0["id"]
                .clone(),
        )
        .unwrap();
        let duplicate = request(
            State(state.clone()),
            Json(req(key, json!({"body":"frozen"}))),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(duplicate["id"], json!(id));
        assert!(
            request(
                State(state.clone()),
                Json(req(key, json!({"body":"changed"})))
            )
            .await
            .is_err()
        );
        let mut forged = req(Uuid::new_v4(), json!({}));
        forged.principal_id = "prn_other".into();
        assert!(request(State(state.clone()), Json(forged)).await.is_err());
        let mut old = req(Uuid::new_v4(), json!({}));
        old.execution_id = "exe_old".into();
        assert!(request(State(state.clone()), Json(old)).await.is_err());
        assert!(
            read(
                State(state.clone()),
                Path(id),
                Json(Identity {
                    sandbox_id: "other".into(),
                    principal_id: "prn_source".into()
                })
            )
            .await
            .is_err()
        );

        // A delivery lease is durable and unique across consumers.
        let delivery = claim_delivery(
            State(state.clone()),
            Json(DeliveryClaim {
                team_id: "TEXAMPLE".into(),
            }),
        )
        .await
        .unwrap()
        .0["delivery"]
            .clone();
        assert!(
            claim_delivery(
                State(state.clone()),
                Json(DeliveryClaim {
                    team_id: "TEXAMPLE".into()
                })
            )
            .await
            .unwrap()
            .0["delivery"]
                .is_null()
        );
        let blocks =
            json!([{"type":"section","text":{"type":"plain_text","text":"full frozen payload"}}]);
        let _ = delivered(
            State(state.clone()),
            Path(id),
            Json(Delivered {
                token: serde_json::from_value(delivery["token"].clone()).unwrap(),
                revision: 1,
                message_ts: "2.000".into(),
                message_blocks: blocks.clone(),
            }),
        )
        .await
        .unwrap();
        let click = |user: &str, decision: &str| Decision {
            team_id: "TEXAMPLE".into(),
            channel_id: "CTEST".into(),
            message_ts: "2.000".into(),
            user_id: user.into(),
            payload_hash: delivery["payload_hash"].as_str().unwrap().into(),
            decision: decision.into(),
            message_blocks: blocks.clone(),
        };
        assert!(
            decide(
                State(state.clone()),
                Path(id),
                Json(click("UMALLORY", "approved"))
            )
            .await
            .is_err()
        );
        let mut changed = click("UALICE", "approved");
        changed.message_ts = "3.000".into();
        assert!(
            decide(State(state.clone()), Path(id), Json(changed))
                .await
                .is_err()
        );
        let mut changed = click("UALICE", "approved");
        changed.payload_hash = "bad".into();
        assert!(
            decide(State(state.clone()), Path(id), Json(changed))
                .await
                .is_err()
        );
        let mut changed = click("UALICE", "approved");
        changed.message_blocks[0]["text"]["text"] = json!("different card");
        assert!(
            decide(State(state.clone()), Path(id), Json(changed))
                .await
                .is_err()
        );
        let (a, b) = tokio::join!(
            decide(
                State(state.clone()),
                Path(id),
                Json(click("UALICE", "approved"))
            ),
            decide(
                State(state.clone()),
                Path(id),
                Json(click("UCAROL", "approved"))
            )
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        // A restarted worker can claim approved work, but racing workers cannot
        // dispatch it twice. The bytes sent to the tool equal the approved bytes.
        let (a, b) = tokio::join!(claim_approved(&state), claim_approved(&state));
        let claims: Vec<_> = a.unwrap().into_iter().chain(b.unwrap()).collect();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].2["arguments"], json!({"body":"frozen"}));
        assert!(claim_approved(&state).await.unwrap().is_empty());
        let waiting: Uuid = serde_json::from_value(
            request(
                State(state.clone()),
                Json(req(Uuid::new_v4(), json!({"queued":true}))),
            )
            .await
            .unwrap()
            .0["id"]
                .clone(),
        )
        .unwrap();
        sqlx::query("update tool_approvals set status='approved' where id=$1")
            .bind(waiting)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            claim_approved(&state).await.unwrap().is_empty(),
            "an executor cannot receive concurrent calls"
        );
        sqlx::query("update tool_approvals set status='declined' where id=$1")
            .bind(waiting)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "update tool_approvals set execution_deadline=now()-interval '1 second' where id=$1",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        assert!(claim_approved(&state).await.unwrap().is_empty());
        assert_eq!(
            read(State(state.clone()), Path(id), Json(identity()))
                .await
                .unwrap()
                .0["status"],
            "unknown"
        );

        let revoked: Uuid = serde_json::from_value(
            request(
                State(state.clone()),
                Json(req(Uuid::new_v4(), json!({"revoked":true}))),
            )
            .await
            .unwrap()
            .0["id"]
                .clone(),
        )
        .unwrap();
        assert!(
            claim_approved(&state.clone().with_approval_policies(BTreeMap::new()))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            read(State(state.clone()), Path(revoked), Json(identity()))
                .await
                .unwrap()
                .0["status"],
            "cancelled"
        );

        for expected in ["expired", "cancelled"] {
            let id: Uuid = serde_json::from_value(
                request(State(state.clone()), Json(req(Uuid::new_v4(), json!({}))))
                    .await
                    .unwrap()
                    .0["id"]
                    .clone(),
            )
            .unwrap();
            if expected == "expired" {
                sqlx::query(
                    "update tool_approvals set expires_at=now()-interval '1 second' where id=$1",
                )
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
            } else {
                sqlx::query("update session_executions set status='cancelled' where execution_id='exe_test'").execute(&pool).await.unwrap();
            }
            assert!(claim_approved(&state).await.unwrap().is_empty());
            assert_eq!(
                read(State(state.clone()), Path(id), Json(identity()))
                    .await
                    .unwrap()
                    .0["status"],
                expected
            );
        }
        pool.close().await;
        sqlx::query(&format!("drop schema {schema} cascade"))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }
}

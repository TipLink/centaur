//! One-shot authorization; scheduling, Slack transport and execution use workflows.
use super::*;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

pub(super) mod repository;
mod runner;
use repository::Repository;
pub(super) use runner::run;

pub const DRIVER_WORKFLOW: &str = "centaur_approval_request";
pub const DECISION_WORKFLOW: &str = "centaur_approval_decision";
pub const EVENT_PREFIX: &str = "centaur.approval:";

pub fn is_reserved(name: &str) -> bool {
    name.starts_with("centaur_approval_")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionPolicy {
    pub workflow: String,
    /// Optional compatibility pin. Admission always freezes the registry OID.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub executor_principal: String,
    pub requester_principals: Vec<String>,
    pub team_id: String,
    pub approver_user_ids: Vec<String>,
    pub allow_self_approval: bool,
    pub expires_seconds: u32,
    pub timeout_seconds: u32,
    pub implementation_revision: String,
    /// Reviewed, non-secret top-level string fields intentionally returned to
    /// the requester and Slack. All other executor output remains private.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub public_result_fields: Vec<String>,
}

pub type Policies = BTreeMap<String, ActionPolicy>;

pub fn parse_policies(raw: &str) -> Result<Policies, WorkflowRuntimeError> {
    let policies: Policies = serde_json::from_str(raw)
        .map_err(|_| WorkflowRuntimeError::BadRequest("invalid approval policy JSON".into()))?;
    for (name, p) in &policies {
        if !identifier(name)
            || !identifier(&p.workflow)
            || is_reserved(&p.workflow)
            || (!p.executor_principal.is_empty() && !p.executor_principal.starts_with("prn_"))
            || p.requester_principals.is_empty()
            || p.requester_principals
                .iter()
                .any(|s| !s.starts_with("prn_") || s == &p.executor_principal)
            || !slack_id(&p.team_id, 'T')
            || p.approver_user_ids.is_empty()
            || p.approver_user_ids
                .iter()
                .any(|s| !slack_id(s, 'U') && !slack_id(s, 'W'))
            || !(30..=86400).contains(&p.expires_seconds)
            || !(1..=300).contains(&p.timeout_seconds)
            || p.implementation_revision.trim().is_empty()
            || p.public_result_fields.len() > 8
            || p.public_result_fields
                .iter()
                .enumerate()
                .any(|(index, field)| {
                    !identifier(field) || p.public_result_fields[..index].contains(field)
                })
        {
            return Err(WorkflowRuntimeError::BadRequest(format!(
                "invalid approval policy for {name}"
            )));
        }
    }
    if policies.values().any(|p| {
        policies
            .values()
            .any(|q| q.requester_principals.contains(&p.executor_principal))
    }) {
        return Err(WorkflowRuntimeError::BadRequest(
            "approval executor is also a requester".into(),
        ));
    }
    Ok(policies)
}

pub fn policies_from_env() -> Result<Policies, WorkflowRuntimeError> {
    parse_policies(&env::var("CENTAUR_TOOL_APPROVAL_POLICIES").unwrap_or_else(|_| "{}".into()))
}

/// Resolve at each boundary; a changed registry binding changes the policy hash
/// and invalidates outstanding requests. Missing/disabled/mismatched executors
/// are unavailable, never a fallback to a shared workflow principal.
pub(super) fn resolve_policies(
    policies: &Policies,
    sandbox: Option<&WorkflowHostSandboxRuntime>,
) -> Policies {
    let Some(sandbox) = sandbox else {
        return Policies::new();
    };
    let Ok(enablement) = WorkflowEnablement::from_env() else {
        return Policies::new();
    };
    let registry = sandbox
        .workflow_principals
        .read()
        .unwrap_or_else(|p| p.into_inner());
    resolve_registry_policies(policies, &registry, &enablement)
}

fn resolve_registry_policies(
    policies: &Policies,
    registry: &WorkflowPrincipalAssignments,
    enablement: &WorkflowEnablement,
) -> Policies {
    policies
        .iter()
        .filter_map(|(name, policy)| {
            if !enablement.is_enabled(&policy.workflow)
                || !registry.approval_required.contains(&policy.workflow)
            {
                return None;
            }
            let principal = registry.registered.get(&policy.workflow)?;
            bind_policy(policy, principal, policies).map(|p| (name.clone(), p))
        })
        .collect()
}

fn bind_policy(
    policy: &ActionPolicy,
    principal: &str,
    policies: &Policies,
) -> Option<ActionPolicy> {
    if !principal.starts_with("prn_")
        || (!policy.executor_principal.is_empty() && policy.executor_principal != principal)
        || policies
            .values()
            .any(|p| p.requester_principals.iter().any(|id| id == principal))
    {
        return None;
    }
    let mut resolved = policy.clone();
    resolved.executor_principal = principal.into();
    Some(resolved)
}

pub(super) fn unavailable() -> WorkflowRuntimeError {
    WorkflowRuntimeError::Disabled("approval unavailable".into())
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
    s.split_once('.').is_some_and(|(a, b)| {
        !a.is_empty()
            && !b.is_empty()
            && s.len() <= 32
            && a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit())
    })
}

fn hash(value: &impl Serialize) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(value).expect("JSON serializable"),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub sandbox_id: String,
    pub principal_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub sandbox_id: String,
    pub principal_id: String,
    pub execution_id: String,
    pub idempotency_key: Uuid,
    pub action: String,
    pub arguments: Value,
}

#[derive(sqlx::FromRow, Serialize)]
struct Context {
    execution_id: String,
    thread_key: String,
    team_id: String,
    channel_id: String,
    requester_id: String,
}

fn thread_timestamp(ctx: &Context) -> Result<String, WorkflowRuntimeError> {
    let qualified = format!("slack:{}:{}:", ctx.team_id, ctx.channel_id);
    let legacy = format!("slack:{}:", ctx.channel_id);
    let ts = ctx
        .thread_key
        .strip_prefix(&qualified)
        .or_else(|| ctx.thread_key.strip_prefix(&legacy))
        .ok_or_else(unavailable)?;
    if !timestamp(ts) {
        return Err(unavailable());
    }
    Ok(ts.into())
}

fn payload(p: &ActionPolicy, arguments: Value) -> Result<(Value, String), WorkflowRuntimeError> {
    if !arguments.is_object() {
        return Err(WorkflowRuntimeError::BadRequest(
            "arguments must be a JSON object".into(),
        ));
    }
    let mut payload = json!({"workflow":p.workflow,"arguments":arguments,
        "executor_principal":p.executor_principal,"implementation_revision":p.implementation_revision});
    if !p.public_result_fields.is_empty() {
        payload["public_result_fields"] = json!(p.public_result_fields);
    }
    let display = display_json(&payload)?;
    if display.len() > 12000 {
        return Err(WorkflowRuntimeError::BadRequest(
            "approval display exceeds 12000 bytes".into(),
        ));
    }
    Ok((payload, display))
}

fn display_json(value: &Value) -> Result<String, WorkflowRuntimeError> {
    let mut display = String::new();
    for ch in serde_json::to_string_pretty(value)?.chars() {
        if !ch.is_ascii() || matches!(ch, '<' | '>' | '&') {
            for unit in ch.encode_utf16(&mut [0; 2]) {
                display.push_str(&format!("\\u{unit:04x}"));
            }
        } else {
            display.push(ch);
        }
    }
    Ok(display)
}

fn visible_blocks(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(visible_blocks).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, v)| {
                    k.as_str() != "block_id" && !(k.as_str() == "emoji" && **v == Value::Bool(true))
                })
                .map(|(k, v)| (k.clone(), visible_blocks(v)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

impl WorkflowRuntime {
    fn approval_repository(&self) -> Repository {
        Repository {
            pool: self.inner.client.pool().clone(),
            policies: Arc::new(resolve_policies(
                &self.inner.approval_policies,
                self.inner.workflow_host_sandbox.as_ref(),
            )),
        }
    }

    pub(super) fn ensure_public_workflow(&self, name: &str) -> Result<(), WorkflowRuntimeError> {
        access::ensure_public(
            name,
            &self.inner.approval_policies,
            self.inner.workflow_host_sandbox.as_ref(),
        )
    }

    pub async fn approval_context(
        &self,
        identity: Identity,
    ) -> Result<Value, WorkflowRuntimeError> {
        self.approval_repository().context(identity).await
    }

    pub async fn request_approval(&self, request: Request) -> Result<Value, WorkflowRuntimeError> {
        let repo = self.approval_repository();
        let p = repo.policies.get(&request.action).ok_or_else(unavailable)?;
        runner::validate_executor(p, self.inner.workflow_host_sandbox.as_ref())?;
        repo.request(request, &self.inner.client).await
    }

    pub async fn read_approval(
        &self,
        id: Uuid,
        identity: Identity,
    ) -> Result<Value, WorkflowRuntimeError> {
        self.approval_repository().read(id, identity).await
    }

    pub async fn cancel_approval(
        &self,
        id: Uuid,
        identity: Identity,
    ) -> Result<Value, WorkflowRuntimeError> {
        self.approval_repository().cancel(id, identity).await
    }

    /// Only accepts a native signature-verified button. The API additionally
    /// requires the Slack ingress identity; ordinary workflow input is never used.
    pub async fn decide_approval(
        &self,
        button: slack_buttons::VerifiedButton,
    ) -> Result<CreateWorkflowRunResponse, WorkflowRuntimeError> {
        self.approval_repository().decide(button).await
    }
}

#[cfg(test)]
mod tests;

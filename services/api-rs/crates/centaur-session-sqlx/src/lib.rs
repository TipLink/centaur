//! SQLx-backed session repository.

use std::{str::FromStr, time::Duration};

use centaur_session_core::{
    ExecutionStatus, HarnessType, MessageRole, SandboxCapabilities, SandboxRepoCacheAccess,
    Session, SessionEvent, SessionExecution, SessionMessage, SessionMessageInput, SessionStatus,
    ThreadKey, empty_object,
};
use serde::Deserialize;
use serde_json::Value;
use sqlx::{
    FromRow, PgPool,
    postgres::{PgListener, PgPoolOptions},
};
use thiserror::Error;
use time::{Duration as TimeDuration, OffsetDateTime};
use uuid::Uuid;

// The API binary embeds these migrations at compile time.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub const SESSION_EVENTS_CHANNEL: &str = "centaur_session_events";
const DEFAULT_MAX_CONNECTIONS: u32 = 500;

#[derive(Clone, Debug)]
pub struct CreateExecutionResult {
    pub execution: SessionExecution,
    pub created: bool,
}

#[derive(Clone, Debug)]
pub struct ClaimExecutionResult {
    pub execution: SessionExecution,
    /// True only when this call transitioned the execution from `queued` to
    /// `running`. False means another request already claimed it (or it is
    /// terminal), so the caller must not drive the execution.
    pub claimed: bool,
}

#[derive(Clone, Debug)]
pub struct RecordExecutionDeliveryResult {
    pub event: SessionEvent,
    /// True only when this call inserted the durable receipt. Replays return
    /// the original event with `created = false`.
    pub created: bool,
}

/// An active execution whose stdout-owner lease was released by
/// [`PgSessionStore::release_stdout_owned_executions`].
#[derive(Clone, Debug)]
pub struct ReleasedExecution {
    pub execution_id: String,
    pub thread_key: ThreadKey,
}

/// An active execution together with its stdout-owner lease state, as
/// returned by [`PgSessionStore::list_active_executions_with_ownership`].
/// The lease snapshot is advisory — only the conditional
/// `claim_expired_stdout_owner` update decides ownership — but it lets an
/// adoption scan skip executions with a live owner without touching the
/// session row or the sandbox backend.
#[derive(Clone, Debug)]
pub struct ActiveExecutionOwnership {
    pub execution: SessionExecution,
    pub stdout_owner_id: Option<String>,
    /// True when a stdout-owner lease exists and has not expired yet.
    pub stdout_owner_lease_active: bool,
}

/// Outcome of the transactional database fence used before stopping a
/// session sandbox. Locking the session row serializes release with execution
/// creation, while the sandbox snapshot prevents an old release request from
/// clearing a newly assigned sandbox.
#[derive(Clone, Debug)]
pub enum ReleaseSessionResult {
    Released {
        session: Box<Session>,
        cancelled_execution: Option<SessionExecution>,
    },
    ActiveExecution(SessionExecution),
    SandboxMismatch {
        current_sandbox_id: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdleSandboxCandidate {
    pub thread_key: ThreadKey,
    pub sandbox_id: String,
    pub execution_id: String,
    pub idle_timeout: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxCapacityCandidate {
    pub thread_key: ThreadKey,
    pub sandbox_id: String,
    pub latest_execution_id: Option<String>,
    pub last_active_at: OffsetDateTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowOwnedSandbox {
    pub thread_key: ThreadKey,
    pub sandbox_id: Option<String>,
}

#[derive(Clone)]
pub struct PgSessionStore {
    pool: PgPool,
}

impl PgSessionStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn connect(database_url: &str) -> Result<Self, SessionStoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(DEFAULT_MAX_CONNECTIONS)
            .connect(database_url)
            .await?;
        Ok(Self::new(pool))
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn run_migrations(&self) -> Result<(), SessionStoreError> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    pub async fn listen_session_events(&self) -> Result<SessionEventListener, SessionStoreError> {
        let mut listener = PgListener::connect_with(&self.pool).await?;
        listener.listen(SESSION_EVENTS_CHANNEL).await?;
        Ok(SessionEventListener { listener })
    }

    pub async fn create_or_get_session(
        &self,
        thread_key: &ThreadKey,
        harness_type: &HarnessType,
        persona_id: Option<&str>,
        metadata: Value,
    ) -> Result<Session, SessionStoreError> {
        sqlx::query(
            r#"
            insert into sessions (thread_key, harness_type, persona_id, status, metadata)
            values ($1, $2, $3, $4, $5)
            on conflict (thread_key) do nothing
            "#,
        )
        .bind(thread_key.as_str())
        .bind(harness_type.as_ref())
        .bind(persona_id)
        .bind(SessionStatus::Idle.as_ref())
        .bind(metadata)
        .execute(&self.pool)
        .await?;

        let session = self.get_session(thread_key).await?;
        if session.harness_type != *harness_type {
            return Err(SessionStoreError::HarnessConflict {
                thread_key: thread_key.as_str().to_owned(),
                existing: session.harness_type.to_string(),
                requested: harness_type.as_ref().to_owned(),
            });
        }
        if session.persona_id.as_deref() != persona_id {
            return Err(SessionStoreError::PersonaConflict {
                thread_key: thread_key.as_str().to_owned(),
                existing: session.persona_id,
                requested: persona_id.map(str::to_owned),
            });
        }
        Ok(session)
    }

    /// Create a child session already bound to an authenticated principal, or
    /// load it only when the existing binding belongs to that same principal.
    /// The principal is written by the insert itself, so a competing creator
    /// cannot observe and claim an unbound row.
    pub async fn create_or_get_session_for_principal(
        &self,
        thread_key: &ThreadKey,
        harness_type: &HarnessType,
        persona_id: Option<&str>,
        metadata: Value,
        iron_control_principal: &str,
    ) -> Result<Session, SessionStoreError> {
        sqlx::query(
            r#"
            insert into sessions (
                thread_key,
                harness_type,
                persona_id,
                status,
                metadata,
                iron_control_principal
            )
            values ($1, $2, $3, $4, $5, $6)
            on conflict (thread_key) do nothing
            "#,
        )
        .bind(thread_key.as_str())
        .bind(harness_type.as_ref())
        .bind(persona_id)
        .bind(SessionStatus::Idle.as_ref())
        .bind(metadata)
        .bind(iron_control_principal)
        .execute(&self.pool)
        .await?;

        let session = self.get_session(thread_key).await?;
        if session.iron_control_principal.as_deref() != Some(iron_control_principal) {
            return Err(SessionStoreError::PrincipalConflict {
                thread_key: thread_key.as_str().to_owned(),
                existing: session.iron_control_principal,
                requested: iron_control_principal.to_owned(),
            });
        }
        if session.harness_type != *harness_type {
            return Err(SessionStoreError::HarnessConflict {
                thread_key: thread_key.as_str().to_owned(),
                existing: session.harness_type.to_string(),
                requested: harness_type.as_ref().to_owned(),
            });
        }
        if session.persona_id.as_deref() != persona_id {
            return Err(SessionStoreError::PersonaConflict {
                thread_key: thread_key.as_str().to_owned(),
                existing: session.persona_id,
                requested: persona_id.map(str::to_owned),
            });
        }
        Ok(session)
    }

    pub async fn get_session(&self, thread_key: &ThreadKey) -> Result<Session, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionRow>(
            r#"
            select thread_key, title, sandbox_id, sandbox_content_revision, sandbox_repo_cache_enabled, sandbox_repo_cache_access, sandbox_observability_enabled, sandbox_api_server_enabled, harness_type, harness_thread_id, persona_id, status, iron_control_principal, sandbox_last_active_at, created_at, updated_at
            from sessions
            where thread_key = $1
            "#,
        )
        .bind(thread_key.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| SessionStoreError::NotFound {
            thread_key: thread_key.as_str().to_owned(),
        })?;

        row.try_into()
    }

    pub async fn get_session_title(
        &self,
        thread_key: &ThreadKey,
    ) -> Result<Option<String>, SessionStoreError> {
        let title = sqlx::query_scalar::<_, Option<String>>(
            r#"
            select title
            from sessions
            where thread_key = $1
            "#,
        )
        .bind(thread_key.as_str())
        .fetch_optional(&self.pool)
        .await?
        .flatten();

        Ok(title)
    }

    pub async fn append_messages(
        &self,
        thread_key: &ThreadKey,
        messages: &[SessionMessageInput],
    ) -> Result<Vec<String>, SessionStoreError> {
        let mut tx = self.pool.begin().await?;
        let mut message_ids = Vec::with_capacity(messages.len());

        for message in messages {
            let message_id = prefixed_id("msg");
            let parts = Value::Array(message.parts.clone());
            let persisted_message_id = sqlx::query_scalar::<_, String>(
                r#"
                insert into session_messages
                    (message_id, thread_key, client_message_id, role, parts, metadata)
                values ($1, $2, $3, $4, $5, $6)
                on conflict (thread_key, client_message_id)
                    where client_message_id is not null
                do update set client_message_id = excluded.client_message_id
                returning message_id
                "#,
            )
            .bind(&message_id)
            .bind(thread_key.as_str())
            .bind(message.client_message_id.as_deref())
            .bind(message.role.as_ref())
            .bind(parts)
            .bind(message.metadata.clone())
            .fetch_one(&mut *tx)
            .await?;
            message_ids.push(persisted_message_id);
        }

        tx.commit().await?;
        Ok(message_ids)
    }

    pub async fn title_generation_candidate(
        &self,
        thread_key: &ThreadKey,
    ) -> Result<Option<Vec<Value>>, SessionStoreError> {
        let rows = sqlx::query_scalar::<_, Value>(
            r#"
            select m.parts
            from sessions s
            join session_messages m on m.thread_key = s.thread_key
            where s.thread_key = $1 and s.title is null
                and m.role = $2
            order by m.created_at, m.message_id
            "#,
        )
        .bind(thread_key.as_str())
        .bind(MessageRole::User.as_ref())
        .fetch_all(&self.pool)
        .await?;

        if rows.is_empty() {
            return Ok(None);
        }

        let parts = rows
            .into_iter()
            .flat_map(|parts| match parts {
                Value::Array(parts) => parts,
                other => vec![other],
            })
            .collect();
        Ok(Some(parts))
    }

    pub async fn set_session_title_if_empty(
        &self,
        thread_key: &ThreadKey,
        title: &str,
    ) -> Result<bool, SessionStoreError> {
        let result = sqlx::query(
            r#"
            update sessions
            set title = $2, updated_at = now()
            where thread_key = $1 and title is null
            "#,
        )
        .bind(thread_key.as_str())
        .bind(title)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn list_messages(
        &self,
        thread_key: &ThreadKey,
    ) -> Result<Vec<SessionMessage>, SessionStoreError> {
        let rows = sqlx::query_as::<_, SessionMessageRow>(
            r#"
            select message_id, client_message_id, thread_key, role, parts, metadata, created_at
            from session_messages
            where thread_key = $1
            order by created_at, message_id
            "#,
        )
        .bind(thread_key.as_str())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn create_execution(
        &self,
        thread_key: &ThreadKey,
        idempotency_key: Option<&str>,
        metadata: Value,
    ) -> Result<CreateExecutionResult, SessionStoreError> {
        let execution_id = prefixed_id("exe");
        let mut tx = self.pool.begin().await?;
        let session_exists =
            sqlx::query_scalar::<_, i32>("select 1 from sessions where thread_key = $1 for update")
                .bind(thread_key.as_str())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if !session_exists {
            return Err(SessionStoreError::NotFound {
                thread_key: thread_key.as_str().to_owned(),
            });
        }
        let row = sqlx::query_as::<_, CreateExecutionRow>(
            r#"
            insert into session_executions
                (execution_id, thread_key, idempotency_key, status, metadata)
            values ($1, $2, $3, $4, $5)
            on conflict (thread_key, idempotency_key)
                where idempotency_key is not null
            do update set idempotency_key = excluded.idempotency_key
            returning
                execution_id = $1 as created,
                execution_id,
                idempotency_key,
                thread_key,
                status,
                metadata,
                error,
                created_at,
                updated_at,
                started_at,
                completed_at
            "#,
        )
        .bind(&execution_id)
        .bind(thread_key.as_str())
        .bind(idempotency_key)
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(metadata)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;

        row.try_into()
    }

    pub async fn release_session_if_sandbox_matches(
        &self,
        thread_key: &ThreadKey,
        expected_sandbox_id: Option<&str>,
        cancel_inflight: bool,
        cancellation_reason: &str,
    ) -> Result<ReleaseSessionResult, SessionStoreError> {
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query_as::<_, SessionRow>(
            r#"
            select thread_key, title, sandbox_id, sandbox_content_revision, sandbox_repo_cache_enabled, sandbox_repo_cache_access, sandbox_observability_enabled, sandbox_api_server_enabled, harness_type, harness_thread_id, persona_id, status, iron_control_principal, sandbox_last_active_at, created_at, updated_at
            from sessions
            where thread_key = $1
            for update
            "#,
        )
        .bind(thread_key.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| SessionStoreError::NotFound {
            thread_key: thread_key.as_str().to_owned(),
        })?;

        if locked.sandbox_id.as_deref() != expected_sandbox_id {
            let current_sandbox_id = locked.sandbox_id;
            tx.commit().await?;
            return Ok(ReleaseSessionResult::SandboxMismatch { current_sandbox_id });
        }

        let active = sqlx::query_as::<_, SessionExecutionRow>(
            r#"
            select execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at
            from session_executions
            where thread_key = $1 and status in ($2, $3)
            order by created_at desc, execution_id desc
            limit 1
            for update
            "#,
        )
        .bind(thread_key.as_str())
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(active) = active.as_ref()
            && !cancel_inflight
        {
            let execution = active.clone().try_into()?;
            tx.commit().await?;
            return Ok(ReleaseSessionResult::ActiveExecution(execution));
        }

        let cancelled_execution = if let Some(active) = active {
            let row = sqlx::query_as::<_, SessionExecutionRow>(
                r#"
                update session_executions
                set status = $2,
                    error = $3,
                    completed_at = coalesce(completed_at, now()),
                    stdout_owner_id = null,
                    stdout_owner_lease_expires_at = null,
                    updated_at = now()
                where execution_id = $1 and status in ($4, $5)
                returning execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at
                "#,
            )
            .bind(active.execution_id)
            .bind(ExecutionStatus::Cancelled.as_ref())
            .bind(cancellation_reason)
            .bind(ExecutionStatus::Queued.as_ref())
            .bind(ExecutionStatus::Running.as_ref())
            .fetch_optional(&mut *tx)
            .await?;
            row.map(TryInto::try_into).transpose()?
        } else {
            None
        };

        let row = sqlx::query_as::<_, SessionRow>(
            r#"
            update sessions
            set sandbox_id = null,
                sandbox_content_revision = null,
                sandbox_repo_cache_enabled = null,
                sandbox_repo_cache_access = null,
                sandbox_observability_enabled = null,
                sandbox_api_server_enabled = null,
                sandbox_last_active_at = null,
                harness_thread_id = null,
                status = $3,
                updated_at = now()
            where thread_key = $1
              and sandbox_id is not distinct from $2
            returning thread_key, title, sandbox_id, sandbox_content_revision, sandbox_repo_cache_enabled, sandbox_repo_cache_access, sandbox_observability_enabled, sandbox_api_server_enabled, harness_type, harness_thread_id, persona_id, status, iron_control_principal, sandbox_last_active_at, created_at, updated_at
            "#,
        )
        .bind(thread_key.as_str())
        .bind(expected_sandbox_id)
        .bind(SessionStatus::Idle.as_ref())
        .fetch_one(&mut *tx)
        .await?;
        let session = row.try_into()?;
        tx.commit().await?;
        Ok(ReleaseSessionResult::Released {
            session: Box::new(session),
            cancelled_execution,
        })
    }

    pub async fn active_execution_for_thread(
        &self,
        thread_key: &ThreadKey,
    ) -> Result<Option<SessionExecution>, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionExecutionRow>(
            r#"
            select execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at
            from session_executions
            where thread_key = $1 and status in ($2, $3)
            order by created_at desc, execution_id desc
            limit 1
            "#,
        )
        .bind(thread_key.as_str())
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .fetch_optional(&self.pool)
        .await?;

        row.map(TryInto::try_into).transpose()
    }

    /// Lists every execution still marked queued or running. Used at startup
    /// to adopt executions orphaned by a previous control plane process.
    pub async fn list_active_executions(&self) -> Result<Vec<SessionExecution>, SessionStoreError> {
        let rows = sqlx::query_as::<_, SessionExecutionRow>(
            r#"
            select execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at
            from session_executions
            where status in ($1, $2)
            order by created_at, execution_id
            "#,
        )
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn list_active_executions_with_ownership(
        &self,
    ) -> Result<Vec<ActiveExecutionOwnership>, SessionStoreError> {
        let rows = sqlx::query_as::<_, ActiveExecutionOwnershipRow>(
            r#"
            select execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at,
                   stdout_owner_id,
                   coalesce(stdout_owner_lease_expires_at > now(), false) as stdout_owner_lease_active
            from session_executions
            where status in ($1, $2)
            order by created_at, execution_id
            "#,
        )
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(ActiveExecutionOwnership {
                    execution: row.execution.try_into()?,
                    stdout_owner_id: row.stdout_owner_id,
                    stdout_owner_lease_active: row.stdout_owner_lease_active,
                })
            })
            .collect()
    }

    pub async fn latest_execution_for_thread(
        &self,
        thread_key: &ThreadKey,
    ) -> Result<Option<SessionExecution>, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionExecutionRow>(
            r#"
            select execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at
            from session_executions
            where thread_key = $1
            order by created_at desc, execution_id desc
            limit 1
            "#,
        )
        .bind(thread_key.as_str())
        .fetch_optional(&self.pool)
        .await?;

        row.map(TryInto::try_into).transpose()
    }

    pub async fn mark_execution_running(
        &self,
        execution_id: &str,
    ) -> Result<ClaimExecutionResult, SessionStoreError> {
        let maybe_row = sqlx::query_as::<_, SessionExecutionRow>(
            r#"
            update session_executions
            set status = $2, started_at = coalesce(started_at, now()), updated_at = now()
            where execution_id = $1 and status = $3
            returning execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at
            "#,
        )
        .bind(execution_id)
        .bind(ExecutionStatus::Running.as_ref())
        .bind(ExecutionStatus::Queued.as_ref())
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = maybe_row else {
            // The execution was not queued: a concurrent request already
            // claimed it or it reached a terminal state. Report the current
            // row without taking ownership.
            let row = sqlx::query_as::<_, SessionExecutionRow>(
                r#"
                select execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at
                from session_executions
                where execution_id = $1
                "#,
            )
            .bind(execution_id)
            .fetch_one(&self.pool)
            .await?;
            return Ok(ClaimExecutionResult {
                execution: row.try_into()?,
                claimed: false,
            });
        };

        self.set_session_status(&row.thread_key, SessionStatus::Executing)
            .await?;
        Ok(ClaimExecutionResult {
            execution: row.try_into()?,
            claimed: true,
        })
    }

    pub async fn claim_stdout_owner(
        &self,
        execution_id: &str,
        owner_id: &str,
        lease: Duration,
    ) -> Result<bool, SessionStoreError> {
        let lease_expires_at = stdout_lease_expires_at(lease);
        let result = sqlx::query(
            r#"
            update session_executions
            set stdout_owner_id = $2,
                stdout_owner_lease_expires_at = $3,
                updated_at = now()
            where execution_id = $1
              and status in ($4, $5)
              and (
                stdout_owner_id is null
                or stdout_owner_id = $2
                or stdout_owner_lease_expires_at < now()
              )
            "#,
        )
        .bind(execution_id)
        .bind(owner_id)
        .bind(lease_expires_at)
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn claim_expired_stdout_owner(
        &self,
        execution_id: &str,
        owner_id: &str,
        lease: Duration,
    ) -> Result<bool, SessionStoreError> {
        let lease_expires_at = stdout_lease_expires_at(lease);
        let result = sqlx::query(
            r#"
            update session_executions
            set stdout_owner_id = $2,
                stdout_owner_lease_expires_at = $3,
                updated_at = now()
            where execution_id = $1
              and status in ($4, $5)
              and (
                stdout_owner_id is null
                or stdout_owner_lease_expires_at < now()
              )
            "#,
        )
        .bind(execution_id)
        .bind(owner_id)
        .bind(lease_expires_at)
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn renew_stdout_owner(
        &self,
        execution_id: &str,
        owner_id: &str,
        lease: Duration,
    ) -> Result<bool, SessionStoreError> {
        let lease_expires_at = stdout_lease_expires_at(lease);
        let result = sqlx::query(
            r#"
            update session_executions
            set stdout_owner_lease_expires_at = $3,
                updated_at = now()
            where execution_id = $1
              and stdout_owner_id = $2
              and status in ($4, $5)
            "#,
        )
        .bind(execution_id)
        .bind(owner_id)
        .bind(lease_expires_at)
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn release_stdout_owner(
        &self,
        execution_id: &str,
        owner_id: &str,
    ) -> Result<bool, SessionStoreError> {
        let result = sqlx::query(
            r#"
            update session_executions
            set stdout_owner_id = null,
                stdout_owner_lease_expires_at = null,
                updated_at = now()
            where execution_id = $1 and stdout_owner_id = $2
            "#,
        )
        .bind(execution_id)
        .bind(owner_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn count_executions_with_stdout_owner(
        &self,
        owner_id: &str,
    ) -> Result<u64, SessionStoreError> {
        let count = sqlx::query_scalar::<_, i64>(
            r#"
            select count(*)
            from session_executions
            where stdout_owner_id = $1 and status in ($2, $3)
            "#,
        )
        .bind(owner_id)
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .fetch_one(&self.pool)
        .await?;

        Ok(u64::try_from(count).unwrap_or_default())
    }

    /// Releases every active stdout-owner lease held by `owner_id` in one
    /// statement, returning the affected executions. Used by a clean
    /// control-plane shutdown so a peer's adoption scan can claim the
    /// executions immediately instead of waiting out the lease TTL.
    pub async fn release_stdout_owned_executions(
        &self,
        owner_id: &str,
    ) -> Result<Vec<ReleasedExecution>, SessionStoreError> {
        let rows = sqlx::query_as::<_, (String, String)>(
            r#"
            update session_executions
            set stdout_owner_id = null,
                stdout_owner_lease_expires_at = null,
                updated_at = now()
            where stdout_owner_id = $1 and status in ($2, $3)
            returning execution_id, thread_key
            "#,
        )
        .bind(owner_id)
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|(execution_id, thread_key)| {
                Ok(ReleasedExecution {
                    execution_id,
                    thread_key: parse_persisted(thread_key)?,
                })
            })
            .collect()
    }

    pub async fn complete_execution(
        &self,
        execution_id: &str,
    ) -> Result<SessionExecution, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionExecutionRow>(
            r#"
            update session_executions
            set status = $2, completed_at = coalesce(completed_at, now()), updated_at = now()
            where execution_id = $1
            returning execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at
            "#,
        )
        .bind(execution_id)
        .bind(ExecutionStatus::Completed.as_ref())
        .fetch_one(&self.pool)
        .await?;

        self.set_session_status(&row.thread_key, SessionStatus::Idle)
            .await?;
        row.try_into()
    }

    pub async fn complete_execution_if_active(
        &self,
        execution_id: &str,
    ) -> Result<Option<SessionExecution>, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionExecutionRow>(
            r#"
            update session_executions
            set status = $2, completed_at = coalesce(completed_at, now()), updated_at = now()
            where execution_id = $1 and status in ($3, $4)
            returning execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at
            "#,
        )
        .bind(execution_id)
        .bind(ExecutionStatus::Completed.as_ref())
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        self.set_session_status(&row.thread_key, SessionStatus::Idle)
            .await?;
        row.try_into().map(Some)
    }

    pub async fn complete_execution_if_active_and_stdout_owner(
        &self,
        execution_id: &str,
        owner_id: &str,
    ) -> Result<Option<SessionExecution>, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionExecutionRow>(
            r#"
            update session_executions
            set status = $2,
                completed_at = coalesce(completed_at, now()),
                stdout_owner_id = null,
                stdout_owner_lease_expires_at = null,
                updated_at = now()
            where execution_id = $1
              and status in ($3, $4)
              and stdout_owner_id = $5
            returning execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at
            "#,
        )
        .bind(execution_id)
        .bind(ExecutionStatus::Completed.as_ref())
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .bind(owner_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        self.set_session_status(&row.thread_key, SessionStatus::Idle)
            .await?;
        row.try_into().map(Some)
    }

    pub async fn fail_execution(
        &self,
        execution_id: &str,
        error: &str,
    ) -> Result<SessionExecution, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionExecutionRow>(
            r#"
            update session_executions
            set status = $2, error = $3, completed_at = coalesce(completed_at, now()), updated_at = now()
            where execution_id = $1
            returning execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at
            "#,
        )
        .bind(execution_id)
        .bind(ExecutionStatus::Failed.as_ref())
        .bind(error)
        .fetch_one(&self.pool)
        .await?;

        self.set_session_status(&row.thread_key, SessionStatus::Failed)
            .await?;
        row.try_into()
    }

    pub async fn fail_execution_if_active(
        &self,
        execution_id: &str,
        error: &str,
    ) -> Result<Option<SessionExecution>, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionExecutionRow>(
            r#"
            update session_executions
            set status = $2, error = $3, completed_at = coalesce(completed_at, now()), updated_at = now()
            where execution_id = $1 and status in ($4, $5)
            returning execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at
            "#,
        )
        .bind(execution_id)
        .bind(ExecutionStatus::Failed.as_ref())
        .bind(error)
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        self.set_session_status(&row.thread_key, SessionStatus::Failed)
            .await?;
        row.try_into().map(Some)
    }

    pub async fn fail_execution_if_active_and_stdout_owner(
        &self,
        execution_id: &str,
        owner_id: &str,
        error: &str,
    ) -> Result<Option<SessionExecution>, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionExecutionRow>(
            r#"
            update session_executions
            set status = $2,
                error = $3,
                completed_at = coalesce(completed_at, now()),
                stdout_owner_id = null,
                stdout_owner_lease_expires_at = null,
                updated_at = now()
            where execution_id = $1
              and status in ($4, $5)
              and stdout_owner_id = $6
            returning execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at
            "#,
        )
        .bind(execution_id)
        .bind(ExecutionStatus::Failed.as_ref())
        .bind(error)
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .bind(owner_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        self.set_session_status(&row.thread_key, SessionStatus::Failed)
            .await?;
        row.try_into().map(Some)
    }

    pub async fn cancel_execution_if_active_and_stdout_owner(
        &self,
        execution_id: &str,
        owner_id: &str,
        reason: &str,
    ) -> Result<Option<SessionExecution>, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionExecutionRow>(
            r#"
            update session_executions
            set status = $2,
                error = $3,
                completed_at = coalesce(completed_at, now()),
                stdout_owner_id = null,
                stdout_owner_lease_expires_at = null,
                updated_at = now()
            where execution_id = $1
              and status in ($4, $5)
              and stdout_owner_id = $6
            returning execution_id, idempotency_key, thread_key, status, metadata, error, created_at, updated_at, started_at, completed_at
            "#,
        )
        .bind(execution_id)
        .bind(ExecutionStatus::Cancelled.as_ref())
        .bind(reason)
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .bind(owner_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        self.set_session_status(&row.thread_key, SessionStatus::Idle)
            .await?;
        row.try_into().map(Some)
    }

    pub async fn append_event(
        &self,
        thread_key: &ThreadKey,
        execution_id: Option<&str>,
        event_type: &str,
        payload: Value,
    ) -> Result<SessionEvent, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionEventRow>(
            r#"
            insert into session_events (thread_key, execution_id, event_type, payload)
            values ($1, $2, $3, $4)
            returning event_id, thread_key, execution_id, event_type, payload, created_at
            "#,
        )
        .bind(thread_key.as_str())
        .bind(execution_id)
        .bind(event_type)
        .bind(payload)
        .fetch_one(&self.pool)
        .await?;

        row.try_into()
    }

    /// Persist one Slack delivery receipt for an exact session execution.
    /// Locking the execution row serializes duplicate callbacks without a new
    /// schema constraint and proves the execution belongs to `thread_key`.
    pub async fn record_execution_delivery(
        &self,
        thread_key: &ThreadKey,
        execution_id: &str,
        event_type: &str,
        payload: Value,
    ) -> Result<Option<RecordExecutionDeliveryResult>, SessionStoreError> {
        let mut tx = self.pool.begin().await?;
        // Match the canonical release transaction's session -> execution lock
        // order. The event insert takes a session FK lock, so locking the
        // execution first could deadlock with a concurrent release.
        let session_exists = sqlx::query_scalar::<_, i32>(
            "select 1 from sessions where thread_key = $1 for key share",
        )
        .bind(thread_key.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !session_exists {
            tx.commit().await?;
            return Ok(None);
        }
        let execution_exists = sqlx::query_scalar::<_, i32>(
            r#"
            select 1
            from session_executions
            where thread_key = $1 and execution_id = $2
            for update
            "#,
        )
        .bind(thread_key.as_str())
        .bind(execution_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !execution_exists {
            tx.commit().await?;
            return Ok(None);
        }

        if let Some(row) = sqlx::query_as::<_, SessionEventRow>(
            r#"
            select event_id, thread_key, execution_id, event_type, payload, created_at
            from session_events
            where execution_id = $1 and event_type = $2
            order by event_id
            limit 1
            "#,
        )
        .bind(execution_id)
        .bind(event_type)
        .fetch_optional(&mut *tx)
        .await?
        {
            tx.commit().await?;
            return Ok(Some(RecordExecutionDeliveryResult {
                event: row.try_into()?,
                created: false,
            }));
        }

        let row = sqlx::query_as::<_, SessionEventRow>(
            r#"
            insert into session_events (thread_key, execution_id, event_type, payload)
            values ($1, $2, $3, $4)
            returning event_id, thread_key, execution_id, event_type, payload, created_at
            "#,
        )
        .bind(thread_key.as_str())
        .bind(execution_id)
        .bind(event_type)
        .bind(payload)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(RecordExecutionDeliveryResult {
            event: row.try_into()?,
            created: true,
        }))
    }

    pub async fn append_event_if_stdout_owner(
        &self,
        thread_key: &ThreadKey,
        execution_id: &str,
        owner_id: &str,
        lease: Duration,
        event_type: &str,
        payload: Value,
    ) -> Result<Option<SessionEvent>, SessionStoreError> {
        let lease_expires_at = stdout_lease_expires_at(lease);
        let mut tx = self.pool.begin().await?;
        // Match the canonical release transaction's session -> execution lock
        // order. Without this key-share lock, output append could lock the
        // execution first and then block on the session FK while release held
        // the session and waited for that execution, producing a deadlock.
        let session_exists = sqlx::query_scalar::<_, i32>(
            "select 1 from sessions where thread_key = $1 for key share",
        )
        .bind(thread_key.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !session_exists {
            tx.commit().await?;
            return Ok(None);
        }
        let result = sqlx::query(
            r#"
            update session_executions
            set stdout_owner_lease_expires_at = $3,
                updated_at = now()
            where execution_id = $1
              and stdout_owner_id = $2
              and status in ($4, $5)
              and thread_key = $6
            "#,
        )
        .bind(execution_id)
        .bind(owner_id)
        .bind(lease_expires_at)
        .bind(ExecutionStatus::Queued.as_ref())
        .bind(ExecutionStatus::Running.as_ref())
        .bind(thread_key.as_str())
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            tx.commit().await?;
            return Ok(None);
        }

        let row = sqlx::query_as::<_, SessionEventRow>(
            r#"
            insert into session_events (thread_key, execution_id, event_type, payload)
            values ($1, $2, $3, $4)
            returning event_id, thread_key, execution_id, event_type, payload, created_at
            "#,
        )
        .bind(thread_key.as_str())
        .bind(execution_id)
        .bind(event_type)
        .bind(payload)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        row.try_into().map(Some)
    }

    pub async fn list_events_after(
        &self,
        thread_key: &ThreadKey,
        after_event_id: i64,
        execution_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SessionEvent>, SessionStoreError> {
        let rows = sqlx::query_as::<_, SessionEventRow>(
            r#"
            select event_id, thread_key, execution_id, event_type, payload, created_at
            from session_events
            where thread_key = $1
              and event_id > $2
              and ($3::text is null or execution_id = $3)
            order by event_id
            limit $4
            "#,
        )
        .bind(thread_key.as_str())
        .bind(after_event_id)
        .bind(execution_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn execution_event_exists(
        &self,
        execution_id: &str,
        event_type: &str,
    ) -> Result<bool, SessionStoreError> {
        let exists = sqlx::query_scalar::<_, bool>(
            r#"
            select exists (
                select 1
                from session_events
                where execution_id = $1
                  and event_type = $2
                limit 1
            )
            "#,
        )
        .bind(execution_id)
        .bind(event_type)
        .fetch_one(&self.pool)
        .await?;

        Ok(exists)
    }

    pub async fn list_referenced_sandbox_ids(&self) -> Result<Vec<String>, SessionStoreError> {
        let rows = sqlx::query_scalar::<_, String>(
            r#"
            select sandbox_id
            from sessions
            where sandbox_id is not null

            union

            select sandbox_id
            from session_warm_sandboxes
            where status in ('ready', 'claimed', 'evicting')
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    pub async fn list_idle_sandbox_candidates(
        &self,
        idle_backstop: Duration,
    ) -> Result<Vec<IdleSandboxCandidate>, SessionStoreError> {
        let rows = sqlx::query_as::<_, IdleSandboxCandidateRow>(
            r#"
            with latest as (
                select distinct on (thread_key)
                    execution_id,
                    thread_key,
                    status,
                    completed_at,
                    metadata
                from session_executions
                order by thread_key, created_at desc, execution_id desc
            )
            select
                s.thread_key,
                s.sandbox_id as sandbox_id,
                latest.execution_id,
                latest.completed_at,
                latest.metadata
            from sessions s
            join latest on latest.thread_key = s.thread_key
            where s.sandbox_id is not null
              and latest.status in ('completed', 'failed', 'cancelled')
              and latest.completed_at is not null
              and not exists (
                  select 1
                  from session_executions active
                  where active.thread_key = s.thread_key
                    and active.status in ('queued', 'running')
              )
            order by latest.completed_at, s.thread_key
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let now = OffsetDateTime::now_utc();
        rows.into_iter()
            .filter_map(|row| idle_candidate_from_row(row, idle_backstop, now).transpose())
            .collect()
    }

    pub async fn list_sandbox_capacity_candidates(
        &self,
        excluded_thread_key: Option<&ThreadKey>,
        hot_idle_grace: std::time::Duration,
        limit: i64,
    ) -> Result<Vec<SandboxCapacityCandidate>, SessionStoreError> {
        let rows = sqlx::query_as::<_, SandboxCapacityCandidateRow>(
            r#"
            with latest as (
                select distinct on (thread_key)
                    execution_id,
                    thread_key,
                    completed_at
                from session_executions
                order by thread_key, created_at desc, execution_id desc
            )
            select
                s.thread_key,
                s.sandbox_id as sandbox_id,
                latest.execution_id as latest_execution_id,
                coalesce(
                    s.sandbox_last_active_at,
                    latest.completed_at,
                    s.updated_at,
                    s.created_at
                ) as last_active_at
            from sessions s
            left join latest on latest.thread_key = s.thread_key
            where s.sandbox_id is not null
              and ($1::text is null or s.thread_key != $1)
              and not exists (
                  select 1
                  from lateral (
                      select e.event_type
                      from session_events e
                      where e.thread_key = s.thread_key
                        and e.payload->>'sandbox_id' = s.sandbox_id
                        and e.event_type in (
                            'session.sandbox_paused',
                            'session.sandbox_ready',
                            'session.sandbox_resumed'
                        )
                      order by e.created_at desc, e.event_id desc
                      limit 1
                  ) latest_sandbox_event
                  where latest_sandbox_event.event_type = 'session.sandbox_paused'
              )
              and coalesce(
                    s.sandbox_last_active_at,
                    latest.completed_at,
                    s.updated_at,
                    s.created_at
                  ) <= now() - ($2::float8 * interval '1 second')
              and not exists (
                  select 1
                  from session_executions active
                  where active.thread_key = s.thread_key
                    and active.status in ('queued', 'running')
              )
            order by last_active_at, s.thread_key
            limit $3
            "#,
        )
        .bind(excluded_thread_key.map(ThreadKey::as_str))
        .bind(hot_idle_grace.as_secs_f64())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn list_workflow_owned_sandboxes(
        &self,
        workflow_run_id: &str,
    ) -> Result<Vec<WorkflowOwnedSandbox>, SessionStoreError> {
        let rows = sqlx::query_as::<_, WorkflowOwnedSandboxRow>(
            r#"
            select thread_key, sandbox_id
            from sessions
            where metadata->>'workflow_owned_thread' = 'true'
              and metadata->>'workflow_run_id' = $1
            order by thread_key
            "#,
        )
        .bind(workflow_run_id)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn update_sandbox_id(
        &self,
        thread_key: &ThreadKey,
        sandbox_id: Option<&str>,
    ) -> Result<Session, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionRow>(
            r#"
            update sessions
            set
                sandbox_id = $2,
                sandbox_content_revision = null,
                sandbox_repo_cache_enabled = null,
                sandbox_repo_cache_access = null,
                sandbox_observability_enabled = null,
                sandbox_api_server_enabled = null,
                sandbox_last_active_at = case
                    when $2::text is null then null
                    else now()
                end,
                updated_at = now()
            where thread_key = $1
            returning thread_key, title, sandbox_id, sandbox_content_revision, sandbox_repo_cache_enabled, sandbox_repo_cache_access, sandbox_observability_enabled, sandbox_api_server_enabled, harness_type, harness_thread_id, persona_id, status, iron_control_principal, sandbox_last_active_at, created_at, updated_at
            "#,
        )
        .bind(thread_key.as_str())
        .bind(sandbox_id)
        .fetch_one(&self.pool)
        .await?;

        row.try_into()
    }

    pub async fn update_sandbox_assignment(
        &self,
        thread_key: &ThreadKey,
        sandbox_id: &str,
        content_revision: Option<&str>,
        capabilities: &SandboxCapabilities,
    ) -> Result<Session, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionRow>(
            r#"
            update sessions
            set
                sandbox_id = $2,
                sandbox_content_revision = $3,
                sandbox_repo_cache_enabled = $4,
                sandbox_repo_cache_access = $5,
                sandbox_observability_enabled = $6,
                sandbox_api_server_enabled = $7,
                sandbox_last_active_at = now(),
                updated_at = now()
            where thread_key = $1
            returning thread_key, title, sandbox_id, sandbox_content_revision, sandbox_repo_cache_enabled, sandbox_repo_cache_access, sandbox_observability_enabled, sandbox_api_server_enabled, harness_type, harness_thread_id, persona_id, status, iron_control_principal, sandbox_last_active_at, created_at, updated_at
            "#,
        )
        .bind(thread_key.as_str())
        .bind(sandbox_id)
        .bind(content_revision)
        .bind(capabilities.repo_cache_enabled())
        .bind(capabilities.repo_cache.as_str())
        .bind(capabilities.observability_enabled)
        .bind(capabilities.api_server_enabled)
        .fetch_one(&self.pool)
        .await?;

        row.try_into()
    }

    /// Bind a sandbox only while the exact execution that allocated it is
    /// still running and the session assignment has not crossed the caller's
    /// fence. Lock order intentionally matches release: session first, then
    /// execution. A concurrent release therefore either clears/cancels first
    /// and this returns `None`, or waits until this assignment commits.
    // These explicit fence fields are kept adjacent to the SQL transaction so
    // a caller cannot accidentally omit ownership, assignment, revision, or
    // capability state while committing a sandbox.
    #[allow(clippy::too_many_arguments)]
    pub async fn assign_sandbox_to_active_execution(
        &self,
        thread_key: &ThreadKey,
        execution_id: &str,
        stdout_owner_id: &str,
        expected_sandbox_id: Option<&str>,
        sandbox_id: &str,
        content_revision: Option<&str>,
        capabilities: &SandboxCapabilities,
    ) -> Result<Option<Session>, SessionStoreError> {
        let mut tx = self.pool.begin().await?;
        let current_sandbox_id = sqlx::query_scalar::<_, Option<String>>(
            r#"
            select sandbox_id
            from sessions
            where thread_key = $1
            for update
            "#,
        )
        .bind(thread_key.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| SessionStoreError::NotFound {
            thread_key: thread_key.as_str().to_owned(),
        })?;
        if current_sandbox_id.as_deref() != expected_sandbox_id {
            tx.commit().await?;
            return Ok(None);
        }

        let execution = sqlx::query_as::<_, (String, bool)>(
            r#"
            select status,
                   coalesce(
                       stdout_owner_id = $3
                       and stdout_owner_lease_expires_at > now(),
                       false
                   ) as owner_active
            from session_executions
            where execution_id = $1 and thread_key = $2
            for update
            "#,
        )
        .bind(execution_id)
        .bind(thread_key.as_str())
        .bind(stdout_owner_id)
        .fetch_optional(&mut *tx)
        .await?;
        if !matches!(
            execution.as_ref(),
            Some((status, true))
                if status == ExecutionStatus::Queued.as_ref()
                    || status == ExecutionStatus::Running.as_ref()
        ) {
            tx.commit().await?;
            return Ok(None);
        }

        let row = sqlx::query_as::<_, SessionRow>(
            r#"
            update sessions
            set
                sandbox_id = $3,
                sandbox_content_revision = $4,
                sandbox_repo_cache_enabled = $5,
                sandbox_repo_cache_access = $6,
                sandbox_observability_enabled = $7,
                sandbox_api_server_enabled = $8,
                sandbox_last_active_at = now(),
                updated_at = now()
            where thread_key = $1
              and sandbox_id is not distinct from $2
            returning thread_key, title, sandbox_id, sandbox_content_revision, sandbox_repo_cache_enabled, sandbox_repo_cache_access, sandbox_observability_enabled, sandbox_api_server_enabled, harness_type, harness_thread_id, persona_id, status, iron_control_principal, sandbox_last_active_at, created_at, updated_at
            "#,
        )
        .bind(thread_key.as_str())
        .bind(expected_sandbox_id)
        .bind(sandbox_id)
        .bind(content_revision)
        .bind(capabilities.repo_cache_enabled())
        .bind(capabilities.repo_cache.as_str())
        .bind(capabilities.observability_enabled)
        .bind(capabilities.api_server_enabled)
        .fetch_optional(&mut *tx)
        .await?;
        let session = row.map(TryInto::try_into).transpose()?;
        tx.commit().await?;
        Ok(session)
    }

    /// Clear an existing sandbox assignment only while the exact execution and
    /// stdout-owner lease that observed it are still active. This is the first
    /// phase of capability replacement: clearing before the external stop
    /// prevents a stale worker from stopping or clearing a recovered worker's
    /// newly assigned sandbox.
    pub async fn clear_sandbox_from_active_execution(
        &self,
        thread_key: &ThreadKey,
        execution_id: &str,
        stdout_owner_id: &str,
        expected_sandbox_id: &str,
    ) -> Result<Option<Session>, SessionStoreError> {
        let mut tx = self.pool.begin().await?;
        let current_sandbox_id = sqlx::query_scalar::<_, Option<String>>(
            r#"
            select sandbox_id
            from sessions
            where thread_key = $1
            for update
            "#,
        )
        .bind(thread_key.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| SessionStoreError::NotFound {
            thread_key: thread_key.as_str().to_owned(),
        })?;
        if current_sandbox_id.as_deref() != Some(expected_sandbox_id) {
            tx.commit().await?;
            return Ok(None);
        }

        let execution = sqlx::query_as::<_, (String, bool)>(
            r#"
            select status,
                   coalesce(
                       stdout_owner_id = $3
                       and stdout_owner_lease_expires_at > now(),
                       false
                   ) as owner_active
            from session_executions
            where execution_id = $1 and thread_key = $2
            for update
            "#,
        )
        .bind(execution_id)
        .bind(thread_key.as_str())
        .bind(stdout_owner_id)
        .fetch_optional(&mut *tx)
        .await?;
        if !matches!(
            execution.as_ref(),
            Some((status, true))
                if status == ExecutionStatus::Queued.as_ref()
                    || status == ExecutionStatus::Running.as_ref()
        ) {
            tx.commit().await?;
            return Ok(None);
        }

        let row = sqlx::query_as::<_, SessionRow>(
            r#"
            update sessions
            set
                sandbox_id = null,
                sandbox_content_revision = null,
                sandbox_repo_cache_enabled = null,
                sandbox_repo_cache_access = null,
                sandbox_observability_enabled = null,
                sandbox_api_server_enabled = null,
                sandbox_last_active_at = null,
                updated_at = now()
            where thread_key = $1 and sandbox_id = $2
            returning thread_key, title, sandbox_id, sandbox_content_revision, sandbox_repo_cache_enabled, sandbox_repo_cache_access, sandbox_observability_enabled, sandbox_api_server_enabled, harness_type, harness_thread_id, persona_id, status, iron_control_principal, sandbox_last_active_at, created_at, updated_at
            "#,
        )
        .bind(thread_key.as_str())
        .bind(expected_sandbox_id)
        .fetch_optional(&mut *tx)
        .await?;
        let session = row.map(TryInto::try_into).transpose()?;
        tx.commit().await?;
        Ok(session)
    }

    pub async fn clear_sandbox_id_if_matches(
        &self,
        thread_key: &ThreadKey,
        sandbox_id: &str,
    ) -> Result<bool, SessionStoreError> {
        let result = sqlx::query(
            r#"
            update sessions
            set
                sandbox_id = null,
                sandbox_content_revision = null,
                sandbox_repo_cache_enabled = null,
                sandbox_repo_cache_access = null,
                sandbox_observability_enabled = null,
                sandbox_api_server_enabled = null,
                sandbox_last_active_at = null,
                updated_at = now()
            where thread_key = $1 and sandbox_id = $2
            "#,
        )
        .bind(thread_key.as_str())
        .bind(sandbox_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Move an existing session onto a different harness. Clears the sandbox
    /// and harness thread state (they belong to the old harness) and resets
    /// the session to idle; messages and events are preserved.
    pub async fn switch_session_harness(
        &self,
        thread_key: &ThreadKey,
        harness_type: &HarnessType,
    ) -> Result<Session, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionRow>(
            r#"
            update sessions
            set harness_type = $2,
                harness_thread_id = null,
                sandbox_id = null,
                sandbox_content_revision = null,
                sandbox_repo_cache_enabled = null,
                sandbox_repo_cache_access = null,
                sandbox_observability_enabled = null,
                sandbox_api_server_enabled = null,
                sandbox_last_active_at = null,
                status = $3,
                updated_at = now()
            where thread_key = $1
            returning thread_key, title, sandbox_id, sandbox_content_revision, sandbox_repo_cache_enabled, sandbox_repo_cache_access, sandbox_observability_enabled, sandbox_api_server_enabled, harness_type, harness_thread_id, persona_id, status, iron_control_principal, sandbox_last_active_at, created_at, updated_at
            "#,
        )
        .bind(thread_key.as_str())
        .bind(harness_type.as_ref())
        .bind(SessionStatus::Idle.as_ref())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| SessionStoreError::NotFound {
            thread_key: thread_key.as_str().to_owned(),
        })?;

        row.try_into()
    }

    pub async fn set_iron_control_principal(
        &self,
        thread_key: &ThreadKey,
        iron_control_principal: Option<&str>,
    ) -> Result<Session, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionRow>(
            r#"
            update sessions
            set iron_control_principal = $2, updated_at = now()
            where thread_key = $1
            returning thread_key, title, sandbox_id, sandbox_content_revision, sandbox_repo_cache_enabled, sandbox_repo_cache_access, sandbox_observability_enabled, sandbox_api_server_enabled, harness_type, harness_thread_id, persona_id, status, iron_control_principal, sandbox_last_active_at, created_at, updated_at
            "#,
        )
        .bind(thread_key.as_str())
        .bind(iron_control_principal)
        .fetch_one(&self.pool)
        .await?;

        row.try_into()
    }

    pub async fn insert_ready_warm_sandbox(
        &self,
        sandbox_id: &str,
        workload_key: &str,
    ) -> Result<(), SessionStoreError> {
        sqlx::query(
            r#"
            insert into session_warm_sandboxes (sandbox_id, workload_key, status)
            values ($1, $2, 'ready')
            "#,
        )
        .bind(sandbox_id)
        .bind(workload_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn count_ready_warm_sandboxes(
        &self,
        workload_key: &str,
    ) -> Result<i64, SessionStoreError> {
        let count = sqlx::query_scalar::<_, i64>(
            r#"
            select count(*)::bigint
            from session_warm_sandboxes
            where workload_key = $1 and status = 'ready'
            "#,
        )
        .bind(workload_key)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    pub async fn list_ready_warm_sandbox_ids(&self) -> Result<Vec<String>, SessionStoreError> {
        let sandbox_ids = sqlx::query_scalar::<_, String>(
            r#"
            select sandbox_id
            from session_warm_sandboxes
            where status = 'ready'
            order by created_at, sandbox_id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(sandbox_ids)
    }

    pub async fn claim_ready_warm_sandbox(
        &self,
        workload_key: &str,
        thread_key: &str,
    ) -> Result<Option<String>, SessionStoreError> {
        let sandbox_id = sqlx::query_scalar::<_, String>(
            r#"
            with candidate as (
                select sandbox_id
                from session_warm_sandboxes
                where workload_key = $1 and status = 'ready'
                order by created_at, sandbox_id
                for update skip locked
                limit 1
            )
            update session_warm_sandboxes warm
            set
                status = 'claimed',
                claimed_thread_key = $2,
                claimed_at = now(),
                updated_at = now()
            from candidate
            where warm.sandbox_id = candidate.sandbox_id
            returning warm.sandbox_id
            "#,
        )
        .bind(workload_key)
        .bind(thread_key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(sandbox_id)
    }

    /// Atomically reserves every unclaimed ready sandbox built for a different
    /// workload. The `status = 'ready'` predicate is repeated on the update so
    /// a concurrent claimant always wins or loses as one transaction; claimed
    /// or otherwise bound sandboxes are never returned for backend eviction.
    pub async fn reserve_ready_warm_sandboxes_for_workload_mismatch(
        &self,
        workload_key: &str,
    ) -> Result<Vec<String>, SessionStoreError> {
        let rows = sqlx::query_scalar::<_, String>(
            r#"
            with candidates as (
                select warm.sandbox_id
                from session_warm_sandboxes warm
                where warm.status = 'ready'
                  and warm.workload_key <> $1
                  and not exists (
                      select 1 from sessions session
                      where session.sandbox_id = warm.sandbox_id
                  )
                order by warm.created_at, warm.sandbox_id
                for update skip locked
            )
            update session_warm_sandboxes warm
            set
                status = 'evicting',
                updated_at = now()
            from candidates
            where warm.sandbox_id = candidates.sandbox_id
              and warm.status = 'ready'
              and not exists (
                  select 1 from sessions session
                  where session.sandbox_id = warm.sandbox_id
              )
            returning warm.sandbox_id
            "#,
        )
        .bind(workload_key)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn reserve_ready_warm_sandboxes_for_eviction(
        &self,
        limit: i64,
    ) -> Result<Vec<String>, SessionStoreError> {
        let rows = sqlx::query_scalar::<_, String>(
            r#"
            with candidates as (
                select sandbox_id
                from session_warm_sandboxes
                where status = 'ready'
                order by created_at, sandbox_id
                for update skip locked
                limit $1
            )
            update session_warm_sandboxes warm
            set
                status = 'evicting',
                updated_at = now()
            from candidates
            where warm.sandbox_id = candidates.sandbox_id
            returning warm.sandbox_id
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn list_stale_evicting_warm_sandbox_ids(
        &self,
        min_age: Duration,
    ) -> Result<Vec<String>, SessionStoreError> {
        let rows = sqlx::query_scalar::<_, String>(
            r#"
            select sandbox_id
            from session_warm_sandboxes
            where status = 'evicting'
              and updated_at <= now() - ($1::float8 * interval '1 second')
            order by updated_at, sandbox_id
            "#,
        )
        .bind(min_age.as_secs_f64())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn mark_warm_sandbox_failed(
        &self,
        sandbox_id: &str,
        error: &str,
    ) -> Result<(), SessionStoreError> {
        sqlx::query(
            r#"
            update session_warm_sandboxes
            set status = 'failed', last_error = $2, updated_at = now()
            where sandbox_id = $1
            "#,
        )
        .bind(sandbox_id)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a stale warm-pool candidate failed only if it is still unclaimed
    /// and unbound. This is the compare-and-swap half of the status probe in
    /// the reconciler: a concurrent session claim must win over stale probe
    /// results collected while the backend call was in flight.
    pub async fn mark_ready_warm_sandbox_failed_if_unclaimed(
        &self,
        sandbox_id: &str,
        error: &str,
    ) -> Result<bool, SessionStoreError> {
        let result = sqlx::query(
            r#"
            update session_warm_sandboxes warm
            set status = 'failed', last_error = $2, updated_at = now()
            where warm.sandbox_id = $1
              and warm.status = 'ready'
              and warm.claimed_thread_key is null
              and not exists (
                  select 1 from sessions session
                  where session.sandbox_id = warm.sandbox_id
              )
            "#,
        )
        .bind(sandbox_id)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn update_harness_thread_id(
        &self,
        thread_key: &ThreadKey,
        harness_thread_id: Option<&str>,
    ) -> Result<Session, SessionStoreError> {
        let row = sqlx::query_as::<_, SessionRow>(
            r#"
            update sessions
            set harness_thread_id = $2, updated_at = now()
            where thread_key = $1
            returning thread_key, title, sandbox_id, sandbox_content_revision, sandbox_repo_cache_enabled, sandbox_repo_cache_access, sandbox_observability_enabled, sandbox_api_server_enabled, harness_type, harness_thread_id, persona_id, status, iron_control_principal, sandbox_last_active_at, created_at, updated_at
            "#,
        )
        .bind(thread_key.as_str())
        .bind(harness_thread_id)
        .fetch_one(&self.pool)
        .await?;

        row.try_into()
    }

    pub async fn touch_session_sandbox_activity(
        &self,
        thread_key: &ThreadKey,
    ) -> Result<bool, SessionStoreError> {
        let result = sqlx::query(
            r#"
            update sessions
            set sandbox_last_active_at = now()
            where thread_key = $1 and sandbox_id is not null
            "#,
        )
        .bind(thread_key.as_str())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn touch_sandbox_activity(
        &self,
        thread_key: &ThreadKey,
        sandbox_id: &str,
    ) -> Result<bool, SessionStoreError> {
        let result = sqlx::query(
            r#"
            update sessions
            set sandbox_last_active_at = now()
            where thread_key = $1 and sandbox_id = $2
            "#,
        )
        .bind(thread_key.as_str())
        .bind(sandbox_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    async fn set_session_status(
        &self,
        thread_key: &str,
        status: SessionStatus,
    ) -> Result<(), SessionStoreError> {
        sqlx::query(
            r#"
            update sessions
            set status = $2, updated_at = now()
            where thread_key = $1
            "#,
        )
        .bind(thread_key)
        .bind(status.as_ref())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

pub struct SessionEventListener {
    listener: PgListener,
}

impl SessionEventListener {
    pub async fn recv(&mut self) -> Result<SessionEventNotification, SessionStoreError> {
        loop {
            let notification = self.listener.recv().await?;
            if notification.channel() != SESSION_EVENTS_CHANNEL {
                continue;
            }

            let payload = notification.payload();
            return serde_json::from_str(payload).map_err(|error| {
                SessionStoreError::InvalidNotification {
                    channel: notification.channel().to_owned(),
                    payload: payload.to_owned(),
                    error,
                }
            });
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SessionEventNotification {
    pub thread_key: String,
    pub event_id: i64,
}

#[derive(Debug, Error)]
pub enum SessionStoreError {
    #[error("session not found for thread_key {thread_key}")]
    NotFound { thread_key: String },
    #[error(
        "session {thread_key} already exists with harness_type {existing}, requested {requested}"
    )]
    HarnessConflict {
        thread_key: String,
        existing: String,
        requested: String,
    },
    #[error(
        "session {thread_key} already exists with persona_id {existing:?}, requested {requested:?}"
    )]
    PersonaConflict {
        thread_key: String,
        existing: Option<String>,
        requested: Option<String>,
    },
    #[error(
        "session {thread_key} is bound to principal {existing:?}, requested principal {requested}"
    )]
    PrincipalConflict {
        thread_key: String,
        existing: Option<String>,
        requested: String,
    },
    #[error("invalid persisted value: {0}")]
    InvalidPersistedValue(String),
    #[error("invalid notification payload on {channel}: {payload}: {error}")]
    InvalidNotification {
        channel: String,
        payload: String,
        error: serde_json::Error,
    },
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

#[derive(Debug, FromRow)]
struct SessionRow {
    thread_key: String,
    title: Option<String>,
    sandbox_id: Option<String>,
    sandbox_content_revision: Option<String>,
    sandbox_repo_cache_enabled: Option<bool>,
    sandbox_repo_cache_access: Option<String>,
    sandbox_observability_enabled: Option<bool>,
    sandbox_api_server_enabled: Option<bool>,
    harness_type: String,
    harness_thread_id: Option<String>,
    persona_id: Option<String>,
    status: String,
    iron_control_principal: Option<String>,
    sandbox_last_active_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl TryFrom<SessionRow> for Session {
    type Error = SessionStoreError;

    fn try_from(row: SessionRow) -> Result<Self, Self::Error> {
        Ok(Self {
            thread_key: parse_persisted(row.thread_key)?,
            title: row.title,
            sandbox_id: row.sandbox_id,
            sandbox_content_revision: row.sandbox_content_revision,
            sandbox_capabilities: match (
                row.sandbox_repo_cache_enabled,
                row.sandbox_repo_cache_access,
                row.sandbox_observability_enabled,
                row.sandbox_api_server_enabled,
            ) {
                (
                    Some(repo_cache_enabled),
                    repo_cache_access,
                    Some(observability_enabled),
                    Some(api_server_enabled),
                ) => Some(SandboxCapabilities {
                    repo_cache: repo_cache_access
                        .as_deref()
                        .and_then(SandboxRepoCacheAccess::parse)
                        .unwrap_or_else(|| {
                            SandboxRepoCacheAccess::from_legacy_enabled(repo_cache_enabled)
                        }),
                    observability_enabled,
                    api_server_enabled,
                }),
                _ => None,
            },
            harness_type: parse_persisted(row.harness_type)?,
            harness_thread_id: row.harness_thread_id,
            persona_id: row.persona_id,
            status: parse_persisted(row.status)?,
            iron_control_principal: row.iron_control_principal,
            sandbox_last_active_at: row.sandbox_last_active_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct SessionMessageRow {
    message_id: String,
    client_message_id: Option<String>,
    thread_key: String,
    role: String,
    parts: Value,
    metadata: Value,
    created_at: OffsetDateTime,
}

impl TryFrom<SessionMessageRow> for SessionMessage {
    type Error = SessionStoreError;

    fn try_from(row: SessionMessageRow) -> Result<Self, Self::Error> {
        let parts = match row.parts {
            Value::Array(parts) => parts,
            other => vec![other],
        };
        Ok(Self {
            message_id: row.message_id,
            client_message_id: row.client_message_id,
            thread_key: parse_persisted(row.thread_key)?,
            role: parse_persisted(row.role)?,
            parts,
            metadata: row.metadata,
            created_at: row.created_at,
        })
    }
}

#[derive(Clone, Debug, FromRow)]
struct SessionExecutionRow {
    execution_id: String,
    idempotency_key: Option<String>,
    thread_key: String,
    status: String,
    metadata: Value,
    error: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    started_at: Option<OffsetDateTime>,
    completed_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct ActiveExecutionOwnershipRow {
    #[sqlx(flatten)]
    execution: SessionExecutionRow,
    stdout_owner_id: Option<String>,
    stdout_owner_lease_active: bool,
}

#[derive(Debug, FromRow)]
struct IdleSandboxCandidateRow {
    thread_key: String,
    sandbox_id: String,
    execution_id: String,
    completed_at: OffsetDateTime,
    metadata: Value,
}

fn idle_candidate_from_row(
    row: IdleSandboxCandidateRow,
    idle_backstop: Duration,
    now: OffsetDateTime,
) -> Result<Option<IdleSandboxCandidate>, SessionStoreError> {
    let idle_timeout = effective_idle_timeout(&row.metadata, idle_backstop);
    if !idle_deadline_elapsed(row.completed_at, idle_timeout, now) {
        return Ok(None);
    }
    Ok(Some(IdleSandboxCandidate {
        thread_key: parse_persisted(row.thread_key)?,
        sandbox_id: row.sandbox_id,
        execution_id: row.execution_id,
        idle_timeout,
    }))
}

fn effective_idle_timeout(metadata: &Value, idle_backstop: Duration) -> Duration {
    metadata
        .get("idle_timeout_ms")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or_else(|| std::cmp::max(idle_backstop, Duration::from_millis(1)))
}

fn idle_deadline_elapsed(
    completed_at: OffsetDateTime,
    idle_timeout: Duration,
    now: OffsetDateTime,
) -> bool {
    let elapsed = now - completed_at;
    if elapsed.is_negative() {
        return false;
    }
    elapsed.whole_nanoseconds() >= idle_timeout.as_nanos() as i128
}

#[derive(Debug, FromRow)]
struct SandboxCapacityCandidateRow {
    thread_key: String,
    sandbox_id: String,
    latest_execution_id: Option<String>,
    last_active_at: OffsetDateTime,
}

impl TryFrom<SandboxCapacityCandidateRow> for SandboxCapacityCandidate {
    type Error = SessionStoreError;

    fn try_from(row: SandboxCapacityCandidateRow) -> Result<Self, Self::Error> {
        Ok(Self {
            thread_key: parse_persisted(row.thread_key)?,
            sandbox_id: row.sandbox_id,
            latest_execution_id: row.latest_execution_id,
            last_active_at: row.last_active_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct WorkflowOwnedSandboxRow {
    thread_key: String,
    sandbox_id: Option<String>,
}

impl TryFrom<WorkflowOwnedSandboxRow> for WorkflowOwnedSandbox {
    type Error = SessionStoreError;

    fn try_from(row: WorkflowOwnedSandboxRow) -> Result<Self, Self::Error> {
        Ok(Self {
            thread_key: parse_persisted(row.thread_key)?,
            sandbox_id: row.sandbox_id,
        })
    }
}

impl TryFrom<SessionExecutionRow> for SessionExecution {
    type Error = SessionStoreError;

    fn try_from(row: SessionExecutionRow) -> Result<Self, Self::Error> {
        Ok(Self {
            execution_id: row.execution_id,
            idempotency_key: row.idempotency_key,
            thread_key: parse_persisted(row.thread_key)?,
            status: parse_persisted(row.status)?,
            metadata: row.metadata,
            error: row.error,
            created_at: row.created_at,
            updated_at: row.updated_at,
            started_at: row.started_at,
            completed_at: row.completed_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct CreateExecutionRow {
    created: bool,
    execution_id: String,
    idempotency_key: Option<String>,
    thread_key: String,
    status: String,
    metadata: Value,
    error: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    started_at: Option<OffsetDateTime>,
    completed_at: Option<OffsetDateTime>,
}

impl TryFrom<CreateExecutionRow> for CreateExecutionResult {
    type Error = SessionStoreError;

    fn try_from(row: CreateExecutionRow) -> Result<Self, Self::Error> {
        Ok(Self {
            created: row.created,
            execution: SessionExecutionRow {
                execution_id: row.execution_id,
                idempotency_key: row.idempotency_key,
                thread_key: row.thread_key,
                status: row.status,
                metadata: row.metadata,
                error: row.error,
                created_at: row.created_at,
                updated_at: row.updated_at,
                started_at: row.started_at,
                completed_at: row.completed_at,
            }
            .try_into()?,
        })
    }
}

#[derive(Debug, FromRow)]
struct SessionEventRow {
    event_id: i64,
    thread_key: String,
    execution_id: Option<String>,
    event_type: String,
    payload: Value,
    created_at: OffsetDateTime,
}

impl TryFrom<SessionEventRow> for SessionEvent {
    type Error = SessionStoreError;

    fn try_from(row: SessionEventRow) -> Result<Self, Self::Error> {
        Ok(Self {
            event_id: row.event_id,
            thread_key: parse_persisted(row.thread_key)?,
            execution_id: row.execution_id,
            event_type: row.event_type,
            payload: row.payload,
            created_at: row.created_at,
        })
    }
}

fn parse_persisted<T>(value: String) -> Result<T, SessionStoreError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|err: T::Err| SessionStoreError::InvalidPersistedValue(err.to_string()))
}

fn prefixed_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

pub fn default_metadata(metadata: Option<Value>) -> Value {
    metadata.unwrap_or_else(empty_object)
}

fn stdout_lease_expires_at(lease: Duration) -> OffsetDateTime {
    let seconds = i64::try_from(lease.as_secs()).unwrap_or(i64::MAX);
    OffsetDateTime::now_utc() + TimeDuration::new(seconds, lease.subsec_nanos() as i32)
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use centaur_session_core::{ExecutionStatus, HarnessType, SandboxCapabilities, ThreadKey};
    use serde_json::json;
    use time::{Duration as TimeDuration, OffsetDateTime};
    use tokio::sync::OnceCell;
    use uuid::Uuid;

    use super::{
        IdleSandboxCandidateRow, PgSessionStore, ReleaseSessionResult, SessionEventNotification,
        SessionStoreError,
    };

    async fn test_store() -> Option<PgSessionStore> {
        let Ok(url) = std::env::var("SESSION_RUNTIME_TEST_DATABASE_URL") else {
            eprintln!("skipping: SESSION_RUNTIME_TEST_DATABASE_URL not set");
            return None;
        };
        static MIGRATIONS: OnceCell<()> = OnceCell::const_new();
        MIGRATIONS
            .get_or_init(|| async {
                let store = PgSessionStore::connect(&url)
                    .await
                    .expect("connect test db");
                store.run_migrations().await.expect("run migrations");
            })
            .await;
        Some(
            PgSessionStore::connect(&url)
                .await
                .expect("connect test db after migrations"),
        )
    }

    #[test]
    fn parses_session_event_notification_payload() {
        let notification: SessionEventNotification =
            serde_json::from_str(r#"{"thread_key":"cli:test","event_id":42}"#).unwrap();

        assert_eq!(
            notification,
            SessionEventNotification {
                thread_key: "cli:test".to_owned(),
                event_id: 42,
            }
        );
    }

    #[tokio::test]
    async fn execution_delivery_receipt_is_thread_bound_and_idempotent() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = ThreadKey::parse(format!("slack:CDELIVERY:{}", Uuid::new_v4())).unwrap();
        let other_thread = ThreadKey::parse(format!("slack:COTHER:{}", Uuid::new_v4())).unwrap();
        for thread in [&thread_key, &other_thread] {
            store
                .create_or_get_session(thread, &HarnessType::Codex, None, json!({}))
                .await
                .expect("create session");
        }
        let execution_id = store
            .create_execution(&thread_key, None, json!({}))
            .await
            .expect("create execution")
            .execution
            .execution_id;

        assert!(
            store
                .record_execution_delivery(
                    &other_thread,
                    &execution_id,
                    "session.delivery_completed",
                    json!({"outcome": "forged"}),
                )
                .await
                .expect("cross-thread record")
                .is_none()
        );

        let first = store
            .record_execution_delivery(
                &thread_key,
                &execution_id,
                "session.delivery_completed",
                json!({"outcome": "primary", "message_id": "1780000000.000100"}),
            )
            .await
            .expect("first receipt")
            .expect("bound execution");
        assert!(first.created);

        let replay = store
            .record_execution_delivery(
                &thread_key,
                &execution_id,
                "session.delivery_completed",
                json!({"outcome": "fallback", "message_id": "different"}),
            )
            .await
            .expect("replayed receipt")
            .expect("bound execution");
        assert!(!replay.created);
        assert_eq!(replay.event.event_id, first.event.event_id);
        assert_eq!(replay.event.payload, first.event.payload);

        let count = sqlx::query_scalar::<_, i64>(
            "select count(*) from session_events where execution_id = $1 and event_type = 'session.delivery_completed'",
        )
        .bind(&execution_id)
        .fetch_one(store.pool())
        .await
        .expect("count receipts");
        assert_eq!(count, 1);
        store
            .complete_execution(&execution_id)
            .await
            .expect("complete first execution before concurrency case");

        let concurrent_execution_id = store
            .create_execution(&thread_key, None, json!({}))
            .await
            .expect("create concurrent execution")
            .execution
            .execution_id;
        let left = store.record_execution_delivery(
            &thread_key,
            &concurrent_execution_id,
            "session.delivery_completed",
            json!({"outcome": "primary", "message_id": "1780000001.000100"}),
        );
        let right = store.record_execution_delivery(
            &thread_key,
            &concurrent_execution_id,
            "session.delivery_completed",
            json!({"outcome": "primary", "message_id": "1780000001.000100"}),
        );
        let (left, right) = tokio::join!(left, right);
        let left = left.expect("left receipt").expect("left execution");
        let right = right.expect("right receipt").expect("right execution");
        assert_ne!(left.created, right.created);
        assert_eq!(left.event.event_id, right.event.event_id);
    }

    #[tokio::test]
    async fn principal_bound_session_rejects_cross_principal_restart_before_mutation() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key =
            ThreadKey::parse(format!("feedback-improvement:test:{}", Uuid::new_v4())).unwrap();
        let created = store
            .create_or_get_session_for_principal(
                &thread_key,
                &HarnessType::Codex,
                None,
                json!({"source": "principal-a"}),
                "prn_a",
            )
            .await
            .expect("create principal A session");
        assert_eq!(created.iron_control_principal.as_deref(), Some("prn_a"));

        let error = store
            .create_or_get_session_for_principal(
                &thread_key,
                &HarnessType::Amp,
                None,
                json!({"source": "principal-b"}),
                "prn_b",
            )
            .await
            .expect_err("principal B must not reach harness restart handling");
        assert!(matches!(
            error,
            SessionStoreError::PrincipalConflict {
                existing: Some(existing),
                requested,
                ..
            } if existing == "prn_a" && requested == "prn_b"
        ));

        let unchanged = store
            .get_session(&thread_key)
            .await
            .expect("principal A session remains");
        assert_eq!(unchanged.harness_type, HarnessType::Codex);
        assert_eq!(unchanged.iron_control_principal.as_deref(), Some("prn_a"));
    }

    fn idle_row(
        metadata: serde_json::Value,
        completed_at: OffsetDateTime,
    ) -> IdleSandboxCandidateRow {
        IdleSandboxCandidateRow {
            thread_key: "test:idle-row".to_owned(),
            sandbox_id: "sbx-idle-row".to_owned(),
            execution_id: "exe-idle-row".to_owned(),
            completed_at,
            metadata,
        }
    }

    #[test]
    fn idle_candidate_uses_persisted_timeout_deadline() {
        let now = OffsetDateTime::now_utc();
        let candidate = super::idle_candidate_from_row(
            idle_row(
                json!({"idle_timeout_ms": 1000}),
                now - TimeDuration::seconds(2),
            ),
            Duration::from_secs(3600),
            now,
        )
        .unwrap()
        .expect("candidate should use persisted timeout");

        assert_eq!(candidate.idle_timeout, Duration::from_secs(1));
    }

    #[test]
    fn idle_candidate_waits_for_persisted_timeout_even_when_backstop_elapsed() {
        let now = OffsetDateTime::now_utc();
        let candidate = super::idle_candidate_from_row(
            idle_row(
                json!({"idle_timeout_ms": 10_000}),
                now - TimeDuration::seconds(2),
            ),
            Duration::from_secs(1),
            now,
        )
        .unwrap();

        assert!(candidate.is_none());
    }

    #[test]
    fn idle_candidate_falls_back_to_backstop_for_missing_or_invalid_timeout() {
        let now = OffsetDateTime::now_utc();
        let candidate = super::idle_candidate_from_row(
            idle_row(
                json!({"idle_timeout_ms": "not-a-number"}),
                now - TimeDuration::seconds(2),
            ),
            Duration::from_secs(1),
            now,
        )
        .unwrap()
        .expect("candidate should use backstop");

        assert_eq!(candidate.idle_timeout, Duration::from_secs(1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_candidates_use_persisted_execution_idle_timeout() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = ThreadKey::parse(format!("test:idle-cleanup-{}", Uuid::new_v4())).unwrap();
        let sandbox_id = format!("sbx-idle-{}", Uuid::new_v4());
        store
            .create_or_get_session(&thread_key, &HarnessType::Codex, None, json!({}))
            .await
            .expect("create session");
        store
            .update_sandbox_id(&thread_key, Some(&sandbox_id))
            .await
            .expect("set sandbox id");
        let execution_id = store
            .create_execution(&thread_key, None, json!({"idle_timeout_ms": 1000}))
            .await
            .expect("create execution")
            .execution
            .execution_id;
        store
            .complete_execution(&execution_id)
            .await
            .expect("complete execution");
        sqlx::query(
            r#"
            update session_executions
            set completed_at = now() - interval '2 seconds', updated_at = now()
            where execution_id = $1
            "#,
        )
        .bind(&execution_id)
        .execute(store.pool())
        .await
        .expect("age execution");

        let candidates = store
            .list_idle_sandbox_candidates(Duration::from_secs(3600))
            .await
            .expect("list idle sandbox candidates");
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.thread_key == thread_key)
            .expect("candidate should use execution idle timeout, not backstop");

        assert_eq!(candidate.sandbox_id, sandbox_id);
        assert_eq!(candidate.execution_id, execution_id);
        assert_eq!(candidate.idle_timeout, Duration::from_secs(1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdout_owner_fences_output_and_terminal_updates() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = ThreadKey::parse(format!("test:stdout-owner-{}", Uuid::new_v4())).unwrap();
        store
            .create_or_get_session(&thread_key, &HarnessType::Codex, None, json!({}))
            .await
            .expect("create session");
        let execution_id = store
            .create_execution(&thread_key, None, json!({}))
            .await
            .expect("create execution")
            .execution
            .execution_id;
        store
            .mark_execution_running(&execution_id)
            .await
            .expect("mark running");

        assert!(
            store
                .claim_stdout_owner(&execution_id, "owner-a", Duration::from_millis(25))
                .await
                .expect("owner-a claims stdout")
        );
        assert!(
            store
                .append_event_if_stdout_owner(
                    &thread_key,
                    &execution_id,
                    "owner-a",
                    Duration::from_millis(25),
                    "session.output.line",
                    json!("line-from-owner-a"),
                )
                .await
                .expect("owner-a appends")
                .is_some()
        );
        assert!(
            store
                .append_event_if_stdout_owner(
                    &thread_key,
                    &execution_id,
                    "owner-b",
                    Duration::from_millis(25),
                    "session.output.line",
                    json!("line-from-stale-owner-b"),
                )
                .await
                .expect("owner-b append is fenced")
                .is_none()
        );
        assert!(
            store
                .complete_execution_if_active_and_stdout_owner(&execution_id, "owner-b")
                .await
                .expect("owner-b terminal update is fenced")
                .is_none()
        );

        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            store
                .claim_expired_stdout_owner(&execution_id, "owner-b", Duration::from_secs(5))
                .await
                .expect("owner-b claims after lease expiry")
        );
        assert!(
            store
                .append_event_if_stdout_owner(
                    &thread_key,
                    &execution_id,
                    "owner-a",
                    Duration::from_secs(5),
                    "session.output.line",
                    json!("line-from-expired-owner-a"),
                )
                .await
                .expect("expired owner-a append is fenced")
                .is_none()
        );
        let completed = store
            .complete_execution_if_active_and_stdout_owner(&execution_id, "owner-b")
            .await
            .expect("owner-b completes")
            .expect("completion should be recorded");
        assert_eq!(
            completed.status,
            centaur_session_core::ExecutionStatus::Completed
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canonical_release_requires_cancellation_and_fences_the_old_stdout_owner() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = ThreadKey::parse(format!("test:release-{}", Uuid::new_v4())).unwrap();
        let sandbox_id = format!("sbx-release-{}", Uuid::new_v4().simple());
        store
            .create_or_get_session(&thread_key, &HarnessType::Codex, None, json!({}))
            .await
            .expect("create session");
        store
            .update_sandbox_id(&thread_key, Some(&sandbox_id))
            .await
            .expect("assign sandbox");
        store
            .update_harness_thread_id(&thread_key, Some("codex-thread-old"))
            .await
            .expect("assign harness thread");
        let execution_id = store
            .create_execution(&thread_key, None, json!({}))
            .await
            .expect("create execution")
            .execution
            .execution_id;
        store
            .mark_execution_running(&execution_id)
            .await
            .expect("mark execution running");
        assert!(
            store
                .claim_stdout_owner(&execution_id, "old-owner", Duration::from_secs(60))
                .await
                .expect("claim stdout owner")
        );

        let rejected = store
            .release_session_if_sandbox_matches(
                &thread_key,
                Some(&sandbox_id),
                false,
                "release requested",
            )
            .await
            .expect("release decision");
        assert!(matches!(rejected, ReleaseSessionResult::ActiveExecution(_)));
        assert_eq!(
            store
                .get_session(&thread_key)
                .await
                .expect("session after rejected release")
                .sandbox_id
                .as_deref(),
            Some(sandbox_id.as_str())
        );

        let released = store
            .release_session_if_sandbox_matches(
                &thread_key,
                Some(&sandbox_id),
                true,
                "release requested",
            )
            .await
            .expect("release session");
        let ReleaseSessionResult::Released {
            session,
            cancelled_execution,
        } = released
        else {
            panic!("expected released session");
        };
        assert_eq!(session.sandbox_id, None);
        assert_eq!(session.harness_thread_id, None);
        assert_eq!(session.status, centaur_session_core::SessionStatus::Idle);
        assert_eq!(
            cancelled_execution
                .as_ref()
                .map(|execution| &execution.status),
            Some(&centaur_session_core::ExecutionStatus::Cancelled)
        );

        assert!(
            store
                .append_event_if_stdout_owner(
                    &thread_key,
                    &execution_id,
                    "old-owner",
                    Duration::from_secs(60),
                    "session.output.line",
                    json!("stale output"),
                )
                .await
                .expect("stale owner append is fenced")
                .is_none()
        );
        assert!(
            store
                .complete_execution_if_active_and_stdout_owner(&execution_id, "old-owner")
                .await
                .expect("stale owner completion is fenced")
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn release_and_stdout_append_share_session_then_execution_lock_order() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = ThreadKey::parse(format!("test:release-race-{}", Uuid::new_v4())).unwrap();
        let sandbox_id = format!("sbx-release-race-{}", Uuid::new_v4().simple());
        store
            .create_or_get_session(&thread_key, &HarnessType::Codex, None, json!({}))
            .await
            .expect("create session");
        store
            .update_sandbox_id(&thread_key, Some(&sandbox_id))
            .await
            .expect("assign sandbox");
        let execution_id = store
            .create_execution(&thread_key, None, json!({}))
            .await
            .expect("create execution")
            .execution
            .execution_id;
        store
            .mark_execution_running(&execution_id)
            .await
            .expect("mark running");
        assert!(
            store
                .claim_stdout_owner(&execution_id, "race-owner", Duration::from_secs(60))
                .await
                .expect("claim owner")
        );

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let append_store = store.clone();
        let append_thread = thread_key.clone();
        let append_execution = execution_id.clone();
        let append_barrier = barrier.clone();
        let append = async move {
            append_barrier.wait().await;
            for sequence in 0..32 {
                if append_store
                    .append_event_if_stdout_owner(
                        &append_thread,
                        &append_execution,
                        "race-owner",
                        Duration::from_secs(60),
                        "session.output.line",
                        json!({"sequence": sequence}),
                    )
                    .await?
                    .is_none()
                {
                    break;
                }
            }
            Ok::<_, SessionStoreError>(())
        };
        let release_store = store.clone();
        let release_thread = thread_key.clone();
        let release_sandbox = sandbox_id.clone();
        let release = async move {
            barrier.wait().await;
            release_store
                .release_session_if_sandbox_matches(
                    &release_thread,
                    Some(&release_sandbox),
                    true,
                    "concurrent release",
                )
                .await
        };

        let (append_result, release_result) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(append, release)
        })
        .await
        .expect("release/stdout append must not deadlock");
        append_result.expect("append result");
        assert!(matches!(
            release_result.expect("release result"),
            ReleaseSessionResult::Released { .. }
        ));
    }

    #[tokio::test]
    async fn cancelled_execution_cannot_assign_a_sandbox_after_null_release() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key =
            ThreadKey::parse(format!("test:release-before-bind-{}", Uuid::new_v4())).unwrap();
        store
            .create_or_get_session(&thread_key, &HarnessType::Codex, None, json!({}))
            .await
            .expect("create session");
        let execution_id = store
            .create_execution(&thread_key, None, json!({}))
            .await
            .expect("create execution")
            .execution
            .execution_id;
        store
            .mark_execution_running(&execution_id)
            .await
            .expect("mark execution running");
        assert!(
            store
                .claim_stdout_owner(&execution_id, "released-owner", Duration::from_secs(60))
                .await
                .expect("claim stdout owner")
        );

        assert!(matches!(
            store
                .release_session_if_sandbox_matches(
                    &thread_key,
                    None,
                    true,
                    "release before sandbox bind",
                )
                .await
                .expect("release session"),
            ReleaseSessionResult::Released { .. }
        ));

        let assigned = store
            .assign_sandbox_to_active_execution(
                &thread_key,
                &execution_id,
                "released-owner",
                None,
                "sbx-too-late",
                None,
                &SandboxCapabilities::default_enabled(),
            )
            .await
            .expect("fenced assignment");
        assert!(assigned.is_none());
        assert_eq!(
            store
                .get_session(&thread_key)
                .await
                .expect("session after fenced assignment")
                .sandbox_id,
            None
        );
        let status = sqlx::query_scalar::<_, String>(
            "select status from session_executions where execution_id = $1",
        )
        .bind(&execution_id)
        .fetch_one(store.pool())
        .await
        .expect("cancelled execution status");
        assert_eq!(status, ExecutionStatus::Cancelled.as_ref());
    }

    #[tokio::test]
    async fn stale_stdout_owner_cannot_assign_or_clear_after_lease_takeover() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key =
            ThreadKey::parse(format!("test:owner-before-bind-{}", Uuid::new_v4())).unwrap();
        store
            .create_or_get_session(&thread_key, &HarnessType::Codex, None, json!({}))
            .await
            .expect("create session");
        let execution_id = store
            .create_execution(&thread_key, None, json!({}))
            .await
            .expect("create execution")
            .execution
            .execution_id;
        store
            .mark_execution_running(&execution_id)
            .await
            .expect("mark execution running");
        assert!(
            store
                .claim_stdout_owner(&execution_id, "owner-a", Duration::from_secs(60))
                .await
                .expect("claim owner A")
        );
        sqlx::query(
            "update session_executions set stdout_owner_lease_expires_at = now() - interval '1 second' where execution_id = $1",
        )
        .bind(&execution_id)
        .execute(store.pool())
        .await
        .expect("expire owner A lease");
        assert!(
            store
                .claim_stdout_owner(&execution_id, "owner-b", Duration::from_secs(60))
                .await
                .expect("claim owner B")
        );

        assert!(
            store
                .assign_sandbox_to_active_execution(
                    &thread_key,
                    &execution_id,
                    "owner-a",
                    None,
                    "sbx-stale-owner",
                    None,
                    &SandboxCapabilities::default_enabled(),
                )
                .await
                .expect("stale owner assignment")
                .is_none()
        );
        assert!(
            store
                .assign_sandbox_to_active_execution(
                    &thread_key,
                    &execution_id,
                    "owner-b",
                    None,
                    "sbx-current-owner",
                    None,
                    &SandboxCapabilities::default_enabled(),
                )
                .await
                .expect("current owner assignment")
                .is_some()
        );
        assert!(
            store
                .clear_sandbox_from_active_execution(
                    &thread_key,
                    &execution_id,
                    "owner-a",
                    "sbx-current-owner",
                )
                .await
                .expect("stale owner clear")
                .is_none()
        );
        assert!(
            store
                .clear_sandbox_from_active_execution(
                    &thread_key,
                    &execution_id,
                    "owner-b",
                    "sbx-current-owner",
                )
                .await
                .expect("current owner clear")
                .is_some()
        );
        assert_eq!(
            store
                .get_session(&thread_key)
                .await
                .expect("session after current owner clear")
                .sandbox_id,
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn releases_all_stdout_leases_held_by_one_owner() {
        let Some(store) = test_store().await else {
            return;
        };
        let owner = format!("owner-{}", Uuid::new_v4().simple());
        let peer = format!("peer-{}", Uuid::new_v4().simple());
        let mut owned = Vec::new();
        for label in ["a", "b"] {
            let thread_key =
                ThreadKey::parse(format!("test:handoff-{label}-{}", Uuid::new_v4())).unwrap();
            store
                .create_or_get_session(&thread_key, &HarnessType::Codex, None, json!({}))
                .await
                .expect("create session");
            let execution_id = store
                .create_execution(&thread_key, None, json!({}))
                .await
                .expect("create execution")
                .execution
                .execution_id;
            store
                .mark_execution_running(&execution_id)
                .await
                .expect("mark running");
            assert!(
                store
                    .claim_stdout_owner(&execution_id, &owner, Duration::from_secs(60))
                    .await
                    .expect("claim stdout owner")
            );
            owned.push((execution_id, thread_key));
        }
        // A bystander owner's lease must survive the release untouched.
        let bystander_thread =
            ThreadKey::parse(format!("test:handoff-bystander-{}", Uuid::new_v4())).unwrap();
        store
            .create_or_get_session(&bystander_thread, &HarnessType::Codex, None, json!({}))
            .await
            .expect("create bystander session");
        let bystander_execution = store
            .create_execution(&bystander_thread, None, json!({}))
            .await
            .expect("create bystander execution")
            .execution
            .execution_id;
        store
            .mark_execution_running(&bystander_execution)
            .await
            .expect("mark bystander running");
        let bystander = format!("bystander-{}", Uuid::new_v4().simple());
        assert!(
            store
                .claim_stdout_owner(&bystander_execution, &bystander, Duration::from_secs(60))
                .await
                .expect("claim bystander lease")
        );
        assert_eq!(
            store
                .count_executions_with_stdout_owner(&owner)
                .await
                .expect("count owned"),
            2
        );

        let released = store
            .release_stdout_owned_executions(&owner)
            .await
            .expect("release owned leases");
        assert_eq!(released.len(), 2);
        for (execution_id, thread_key) in &owned {
            assert!(
                released.iter().any(|execution| {
                    execution.execution_id == *execution_id && execution.thread_key == *thread_key
                }),
                "released set must include {execution_id}"
            );
        }
        assert_eq!(
            store
                .count_executions_with_stdout_owner(&owner)
                .await
                .expect("count after release"),
            0
        );

        // Released leases are immediately claimable by a peer, without
        // waiting for expiry.
        assert!(
            store
                .claim_stdout_owner(&owned[0].0, &peer, Duration::from_secs(60))
                .await
                .expect("peer claims released lease")
        );

        assert_eq!(
            store
                .count_executions_with_stdout_owner(&bystander)
                .await
                .expect("count bystander"),
            1,
            "release must be scoped to the requested owner"
        );
        store
            .fail_execution_if_active(&bystander_execution, "test cleanup")
            .await
            .expect("terminalize bystander");

        // Terminal executions are never part of a release, even if a lease
        // column is still populated.
        for (execution_id, _) in &owned {
            store
                .fail_execution_if_active(execution_id, "test cleanup")
                .await
                .expect("terminalize execution");
        }
        assert!(
            store
                .release_stdout_owned_executions(&peer)
                .await
                .expect("release for peer")
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warm_eviction_reservation_blocks_later_claims() {
        let Some(store) = test_store().await else {
            return;
        };
        let sandbox_id = format!("sbx-warm-evict-{}", Uuid::new_v4());
        let workload_key = format!("workload-warm-evict-{}", Uuid::new_v4());
        store
            .insert_ready_warm_sandbox(&sandbox_id, &workload_key)
            .await
            .expect("insert warm sandbox");
        sqlx::query(
            r#"
            update session_warm_sandboxes
            set created_at = now() - interval '100 years'
            where sandbox_id = $1
            "#,
        )
        .bind(&sandbox_id)
        .execute(store.pool())
        .await
        .expect("age warm sandbox");

        let reserved = store
            .reserve_ready_warm_sandboxes_for_eviction(1)
            .await
            .expect("reserve warm sandbox");

        assert_eq!(reserved, vec![sandbox_id.clone()]);
        assert_eq!(
            store
                .claim_ready_warm_sandbox(&workload_key, "test-thread")
                .await
                .expect("claim after reservation"),
            None
        );
        assert!(
            store
                .list_referenced_sandbox_ids()
                .await
                .expect("list referenced sandboxes")
                .contains(&sandbox_id)
        );

        store
            .mark_warm_sandbox_failed(&sandbox_id, "test cleanup")
            .await
            .expect("mark reserved warm sandbox failed");
        assert!(
            !store
                .list_referenced_sandbox_ids()
                .await
                .expect("list referenced sandboxes")
                .contains(&sandbox_id)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_ready_failure_cannot_overwrite_a_concurrent_claim() {
        let Some(store) = test_store().await else {
            return;
        };
        let sandbox_id = format!("sbx-warm-claim-race-{}", Uuid::new_v4());
        let workload_key = format!("workload-warm-claim-race-{}", Uuid::new_v4());
        let thread_key = ThreadKey::parse(format!("test:warm-claim-race-{}", Uuid::new_v4()))
            .expect("thread key");
        store
            .create_or_get_session(&thread_key, &HarnessType::Codex, None, json!({}))
            .await
            .expect("create session");
        store
            .insert_ready_warm_sandbox(&sandbox_id, &workload_key)
            .await
            .expect("insert warm sandbox");
        assert_eq!(
            store
                .claim_ready_warm_sandbox(&workload_key, thread_key.as_str())
                .await
                .expect("claim warm sandbox"),
            Some(sandbox_id.clone())
        );

        assert!(
            !store
                .mark_ready_warm_sandbox_failed_if_unclaimed(&sandbox_id, "stale backend status",)
                .await
                .expect("conditional stale failure")
        );
        let status = sqlx::query_scalar::<_, String>(
            "select status from session_warm_sandboxes where sandbox_id = $1",
        )
        .bind(&sandbox_id)
        .fetch_one(store.pool())
        .await
        .expect("load warm status");
        assert_eq!(status, "claimed");

        store
            .mark_warm_sandbox_failed(&sandbox_id, "test cleanup")
            .await
            .expect("cleanup warm sandbox");
    }
}

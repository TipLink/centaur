use centaur_session_core::user_input::{
    SessionUserInputRequest, UserInputAnswers, UserInputQuestion, validate_user_input_answers,
};
use serde_json::json;
use sqlx::{FromRow, types::Json};

use super::{PgSessionStore, SessionStoreError, ThreadKey, insert_session_event};

pub enum UserInputClaim {
    Accepted(SessionUserInputRequest),
    NotFound,
    Forbidden,
    Conflict(String),
    Invalid(String),
}

#[derive(FromRow)]
struct RequestRow {
    request_id: String,
    execution_id: String,
    requester_user_id: Option<String>,
    questions: Json<Vec<UserInputQuestion>>,
    status: String,
    answers: Option<Json<UserInputAnswers>>,
}

impl From<RequestRow> for SessionUserInputRequest {
    fn from(row: RequestRow) -> Self {
        Self {
            request_id: row.request_id,
            execution_id: row.execution_id,
            requester_user_id: row.requester_user_id,
            questions: row.questions.0,
            status: row.status,
            answers: row.answers.map(|answers| answers.0),
        }
    }
}

impl PgSessionStore {
    /// Persist the request and its replay event together before an ingress can render it.
    pub async fn record_user_input_request(
        &self,
        thread_key: &ThreadKey,
        request: &SessionUserInputRequest,
        stdout_owner_id: &str,
    ) -> Result<(), SessionStoreError> {
        let mut tx = self.pool.begin().await?;
        let running = sqlx::query_scalar::<_, String>(
            "select status from session_executions where execution_id = $1 and thread_key = $2
             and stdout_owner_id = $3 and status = 'running' for update",
        )
        .bind(&request.execution_id)
        .bind(thread_key.as_str())
        .bind(stdout_owner_id)
        .fetch_optional(&mut *tx)
        .await?;
        if running.is_none() {
            return Ok(());
        }
        let inserted = sqlx::query(
            "insert into session_user_input_requests
             (request_id, thread_key, execution_id, requester_user_id, questions)
             values ($1, $2, $3, $4, $5) on conflict (request_id) do nothing",
        )
        .bind(&request.request_id)
        .bind(thread_key.as_str())
        .bind(&request.execution_id)
        .bind(&request.requester_user_id)
        .bind(Json(&request.questions))
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 1 {
            insert_session_event(
                &mut *tx,
                thread_key,
                Some(&request.execution_id),
                "session.user_input.requested",
                json!(request),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_user_input_requests(
        &self,
        thread_key: &ThreadKey,
    ) -> Result<Vec<SessionUserInputRequest>, SessionStoreError> {
        let rows = sqlx::query_as::<_, RequestRow>(
            "select r.request_id, r.execution_id, r.requester_user_id, r.questions, r.status, r.answers
             from session_user_input_requests r join session_executions e using (execution_id)
             where r.thread_key = $1 and r.status in ('pending','submitted') and e.status = 'running'
             order by r.created_at",
        ).bind(thread_key.as_str()).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_user_input_request(
        &self,
        thread_key: &ThreadKey,
        request_id: &str,
    ) -> Result<Option<SessionUserInputRequest>, SessionStoreError> {
        let row = sqlx::query_as::<_, RequestRow>(
            "select request_id, execution_id, requester_user_id, questions, status, answers
             from session_user_input_requests where thread_key = $1 and request_id = $2",
        )
        .bind(thread_key.as_str())
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Claims immutable answers before writing to the harness. Same-answer retries
    /// are allowed; the live harness deduplicates the opaque request id.
    pub async fn claim_user_input_answers(
        &self,
        thread_key: &ThreadKey,
        execution_id: &str,
        request_id: &str,
        responder_user_id: &str,
        answers: &UserInputAnswers,
    ) -> Result<UserInputClaim, SessionStoreError> {
        let mut tx = self.pool.begin().await?;
        // All question writes take the execution lock first, matching terminal cleanup.
        let execution_status = sqlx::query_scalar::<_, String>(
            "select status from session_executions where execution_id = $1 and thread_key = $2 for update",
        ).bind(execution_id).bind(thread_key.as_str()).fetch_optional(&mut *tx).await?;
        let Some(row) = sqlx::query_as::<_, RequestRow>(
            "select request_id, execution_id, requester_user_id, questions, status, answers
             from session_user_input_requests where thread_key = $1 and request_id = $2 for update",
        )
        .bind(thread_key.as_str())
        .bind(request_id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            return Ok(UserInputClaim::NotFound);
        };
        let mut request: SessionUserInputRequest = row.into();
        if request.requester_user_id.as_deref() != Some(responder_user_id)
            || responder_user_id.is_empty()
        {
            return Ok(UserInputClaim::Forbidden);
        }
        if request.execution_id != execution_id {
            return Ok(UserInputClaim::Conflict(
                "request belongs to another execution".to_owned(),
            ));
        }
        if let Err(error) = validate_user_input_answers(&request.questions, answers) {
            return Ok(UserInputClaim::Invalid(error));
        }
        if request
            .answers
            .as_ref()
            .is_some_and(|existing| existing != answers)
        {
            return Ok(UserInputClaim::Conflict(
                "different answers were already submitted".to_owned(),
            ));
        }
        if request.status == "resolved" && request.answers.as_ref() == Some(answers) {
            return Ok(UserInputClaim::Accepted(request));
        }
        if execution_status.as_deref() != Some("running") || request.status == "cancelled" {
            return Ok(UserInputClaim::Conflict(
                "question is no longer active".to_owned(),
            ));
        }
        sqlx::query(
            "update session_user_input_requests set answers = $2, status = 'submitted', updated_at = now()
             where request_id = $1",
        ).bind(request_id).bind(Json(answers)).execute(&mut *tx).await?;
        tx.commit().await?;
        request.answers = Some(answers.clone());
        request.status = "submitted".to_owned();
        Ok(UserInputClaim::Accepted(request))
    }

    pub async fn resolve_user_input_request(
        &self,
        thread_key: &ThreadKey,
        execution_id: &str,
        request_id: &str,
        stdout_owner_id: &str,
        answered: bool,
    ) -> Result<(), SessionStoreError> {
        let mut tx = self.pool.begin().await?;
        let owner = sqlx::query_scalar::<_, String>(
            "select execution_id from session_executions where execution_id = $1 and thread_key = $2
             and stdout_owner_id = $3 and status = 'running' for update",
        ).bind(execution_id).bind(thread_key.as_str()).bind(stdout_owner_id)
        .fetch_optional(&mut *tx).await?;
        if owner.is_none() {
            return Ok(());
        }
        let status = if answered { "resolved" } else { "cancelled" };
        let changed = sqlx::query(
            "update session_user_input_requests set status = $4, updated_at = now()
             where request_id = $1 and execution_id = $2 and thread_key = $3
             and status in ('pending','submitted') and (not $5 or answers is not null)",
        )
        .bind(request_id)
        .bind(execution_id)
        .bind(thread_key.as_str())
        .bind(status)
        .bind(answered)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() == 1 {
            insert_session_event(
                &mut *tx,
                thread_key,
                Some(execution_id),
                "session.user_input.resolved",
                json!({"request_id":request_id,"execution_id":execution_id,"status":status,
                       "reason":if answered { "answered" } else { "cancelled" }}),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use centaur_session_core::HarnessType;
    use std::time::Duration;
    use uuid::Uuid;

    async fn fixture() -> Option<(PgSessionStore, ThreadKey, SessionUserInputRequest)> {
        let url = std::env::var("SESSION_RUNTIME_TEST_DATABASE_URL").ok()?;
        let store = PgSessionStore::connect(&url).await.unwrap();
        store
            .run_migrations(crate::TextSearchBackend::Postgres)
            .await
            .unwrap();
        let thread = ThreadKey::parse(format!("test:questions-{}", Uuid::new_v4())).unwrap();
        store
            .create_or_get_session(
                &thread,
                &HarnessType::Codex,
                None,
                json!({}),
                Default::default(),
            )
            .await
            .unwrap();
        let execution = store
            .create_execution(&thread, None, json!({"user_id":"U1"}))
            .await
            .unwrap()
            .execution;
        store
            .mark_execution_running(&execution.execution_id)
            .await
            .unwrap();
        store
            .claim_stdout_owner(&execution.execution_id, "owner", Duration::from_secs(60))
            .await
            .unwrap();
        let request = SessionUserInputRequest {
            request_id: Uuid::new_v4().to_string(),
            execution_id: execution.execution_id,
            requester_user_id: Some("U1".to_owned()),
            questions: serde_json::from_value(json!([{"id":"project","question":"Project?",
                "options":[{"label":"A"},{"label":"B"}]}]))
            .unwrap(),
            status: "pending".to_owned(),
            answers: None,
        };
        Some((store, thread, request))
    }

    #[tokio::test]
    async fn user_input_claim_is_durable_requester_bound_and_idempotent() {
        let Some((store, thread, request)) = fixture().await else {
            return;
        };
        store
            .record_user_input_request(&thread, &request, "stale-owner")
            .await
            .unwrap();
        assert!(
            store
                .list_user_input_requests(&thread)
                .await
                .unwrap()
                .is_empty()
        );
        store
            .record_user_input_request(&thread, &request, "owner")
            .await
            .unwrap();
        store
            .record_user_input_request(&thread, &request, "owner")
            .await
            .unwrap();
        let events = store
            .list_events_after(&thread, 0, None, 100)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "session.user_input.requested")
                .count(),
            1
        );
        let answers: UserInputAnswers =
            serde_json::from_value(json!({"project":{"answers":["A"]}})).unwrap();
        assert!(matches!(
            store
                .claim_user_input_answers(
                    &thread,
                    &request.execution_id,
                    &request.request_id,
                    "U2",
                    &answers
                )
                .await
                .unwrap(),
            UserInputClaim::Forbidden
        ));
        assert!(matches!(
            store
                .claim_user_input_answers(
                    &thread,
                    "another-execution",
                    &request.request_id,
                    "U1",
                    &answers
                )
                .await
                .unwrap(),
            UserInputClaim::Conflict(_)
        ));
        let first = store.claim_user_input_answers(
            &thread,
            &request.execution_id,
            &request.request_id,
            "U1",
            &answers,
        );
        let second = store.claim_user_input_answers(
            &thread,
            &request.execution_id,
            &request.request_id,
            "U1",
            &answers,
        );
        let (first, second) = tokio::join!(first, second);
        assert!(matches!(first.unwrap(), UserInputClaim::Accepted(_)));
        assert!(matches!(second.unwrap(), UserInputClaim::Accepted(_)));
        // A new repository instance sees the same committed answer.
        let reopened = PgSessionStore::new(store.pool().clone());
        assert_eq!(
            reopened
                .get_user_input_request(&thread, &request.request_id)
                .await
                .unwrap()
                .unwrap()
                .answers,
            Some(answers.clone())
        );
        let changed: UserInputAnswers =
            serde_json::from_value(json!({"project":{"answers":["B"]}})).unwrap();
        assert!(matches!(
            store
                .claim_user_input_answers(
                    &thread,
                    &request.execution_id,
                    &request.request_id,
                    "U1",
                    &changed
                )
                .await
                .unwrap(),
            UserInputClaim::Conflict(_)
        ));
        store
            .resolve_user_input_request(
                &thread,
                &request.execution_id,
                &request.request_id,
                "owner",
                true,
            )
            .await
            .unwrap();
        store
            .resolve_user_input_request(
                &thread,
                &request.execution_id,
                &request.request_id,
                "owner",
                true,
            )
            .await
            .unwrap();
        store
            .complete_execution(&request.execution_id)
            .await
            .unwrap();
        assert!(
            matches!(store.claim_user_input_answers(&thread, &request.execution_id, &request.request_id, "U1", &answers).await.unwrap(), UserInputClaim::Accepted(request) if request.status == "resolved")
        );
        let events = store
            .list_events_after(&thread, 0, None, 100)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "session.user_input.resolved")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn terminal_execution_cancels_questions_and_replay_cannot_reopen_them() {
        let Some((store, thread, request)) = fixture().await else {
            return;
        };
        store
            .record_user_input_request(&thread, &request, "owner")
            .await
            .unwrap();
        store
            .complete_execution(&request.execution_id)
            .await
            .unwrap();
        store
            .record_user_input_request(&thread, &request, "owner")
            .await
            .unwrap();
        assert!(
            store
                .list_user_input_requests(&thread)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .get_user_input_request(&thread, &request.request_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "cancelled"
        );
        let answers: UserInputAnswers =
            serde_json::from_value(json!({"project":{"answers":["A"]}})).unwrap();
        assert!(matches!(
            store
                .claim_user_input_answers(
                    &thread,
                    &request.execution_id,
                    &request.request_id,
                    "U1",
                    &answers
                )
                .await
                .unwrap(),
            UserInputClaim::Conflict(_)
        ));
        let events = store
            .list_events_after(&thread, 0, None, 100)
            .await
            .unwrap();
        let cancelled = events
            .iter()
            .find(|event| event.event_type == "session.user_input.resolved")
            .unwrap();
        assert_eq!(cancelled.payload["reason"], "execution_ended");
    }
}

//! A turn-scoped bridge for native harness questions. The control plane owns
//! persistence; this map only correlates a live native request with its reply.
use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{HarnessServerError, Result};

pub(crate) const QUESTION_GUIDANCE: &str = "When a requester decision is needed to continue, use the native question tool, group related choices, and resume the task after the answers; reuse information already provided.";

pub type Answers = BTreeMap<String, UserInputAnswer>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputAnswer {
    pub answers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserInputResponse {
    pub request_id: String,
    pub execution_id: String,
    pub answers: Answers,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default = "yes")]
    pub allow_other: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct NativeUserInputRequest {
    pub native_id: Value,
    pub item_id: String,
    pub questions: Vec<Question>,
    pub input: Value,
    pub is_blocking: bool,
}

/// Controls that an adapter handles before its ordinary output normalizer.
pub enum UserInputControl {
    Request(NativeUserInputRequest),
    Cancel(Value),
    Reply(Vec<u8>),
}

pub(crate) struct PendingQuestions {
    execution_id: String,
    enabled: bool,
    pending: HashMap<String, NativeUserInputRequest>,
    answered: HashMap<String, Answers>,
}

impl PendingQuestions {
    pub(crate) fn new(execution_id: Option<&str>, enabled: bool) -> Self {
        Self {
            execution_id: execution_id.unwrap_or_default().to_owned(),
            enabled,
            pending: HashMap::new(),
            answered: HashMap::new(),
        }
    }

    pub(crate) fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(crate) fn register(
        &mut self,
        request: NativeUserInputRequest,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<Value> {
        if !self.enabled {
            return Err(HarnessServerError::Protocol(
                "calling ingress does not support native questions".into(),
            ));
        }
        validate_questions(&request.questions)?;
        if !request.native_id.is_i64()
            && !request
                .native_id
                .as_str()
                .is_some_and(|id| !id.is_empty() && id.len() <= 256)
        {
            return Err(HarnessServerError::Protocol(
                "invalid native user input request id".into(),
            ));
        }
        if self.pending.len() + self.answered.len() >= 64 {
            return Err(HarnessServerError::Protocol(
                "too many user input requests in one turn".into(),
            ));
        }
        if self
            .pending
            .values()
            .any(|existing| existing.native_id == request.native_id)
        {
            return Err(HarnessServerError::Protocol(
                "duplicate native user input request".into(),
            ));
        }
        // Native ids can repeat after a CLI restart. Never expose them as the
        // durable answer key: a late click must not answer a different call.
        let request_id = Uuid::new_v4().to_string();
        let wire = json!({"id":request_id,"method":"item/tool/requestUserInput","params":{
            "requestId":request_id,"threadId":thread_id,"turnId":turn_id,
            "itemId":request.item_id,"questions":request.questions,"isBlocking":request.is_blocking
        }});
        self.pending.insert(request_id, request);
        Ok(wire)
    }

    /// None means a matching response was already written. A failed native
    /// write is not marked answered, so the runtime can retry the same reply.
    pub(crate) fn validate_response(
        &self,
        response: &UserInputResponse,
    ) -> Result<Option<&NativeUserInputRequest>> {
        if response.execution_id != self.execution_id {
            return Err(HarnessServerError::Protocol(
                "user input response belongs to another execution".into(),
            ));
        }
        if let Some(answers) = self.answered.get(&response.request_id) {
            return if answers == &response.answers {
                Ok(None)
            } else {
                Err(HarnessServerError::Protocol(
                    "user input request was already answered differently".into(),
                ))
            };
        }
        let request = self.pending.get(&response.request_id).ok_or_else(|| {
            HarnessServerError::Protocol("user input request is no longer pending".into())
        })?;
        if response.answers.len() != request.questions.len() {
            return invalid_answer();
        }
        for question in &request.questions {
            let Some(answer) = response.answers.get(&question.id) else {
                return invalid_answer();
            };
            if answer.answers.is_empty()
                || answer.answers.len() > 16
                || (!question.multi_select && answer.answers.len() != 1)
            {
                return invalid_answer();
            }
            let mut seen = HashSet::new();
            for text in &answer.answers {
                if text.trim().is_empty()
                    || text.len() > 16_000
                    || !seen.insert(text)
                    || (!question.allow_other
                        && !question.options.iter().any(|option| &option.label == text))
                {
                    return invalid_answer();
                }
            }
        }
        Ok(Some(request))
    }

    pub(crate) fn answered(&mut self, response: UserInputResponse) -> Value {
        self.pending.remove(&response.request_id);
        self.answered
            .insert(response.request_id.clone(), response.answers);
        resolved(&response.request_id, "answered")
    }

    /// A process replacement loses the native callback. Tell the durable
    /// caller that a reply for this execution cannot reach its old request.
    /// Never let a foreign execution cancel a request in the current one.
    pub(crate) fn stale_response(&self, response: &UserInputResponse) -> Option<Value> {
        (response.execution_id == self.execution_id
            && !self.pending.contains_key(&response.request_id)
            && !self.answered.contains_key(&response.request_id))
        .then(|| resolved(&response.request_id, "cancelled"))
    }

    pub(crate) fn cancel_native(&mut self, native_id: &Value) -> Option<Value> {
        let id = self
            .pending
            .iter()
            .find(|(_, request)| &request.native_id == native_id)?
            .0
            .clone();
        self.pending.remove(&id);
        Some(resolved(&id, "cancelled"))
    }

    pub(crate) fn cancel_all(&mut self) -> Vec<Value> {
        self.pending
            .drain()
            .map(|(id, _)| resolved(&id, "cancelled"))
            .collect()
    }
}

fn invalid_answer<T>() -> Result<T> {
    Err(HarnessServerError::Protocol(
        "invalid or incomplete user input answers".into(),
    ))
}

fn validate_questions(questions: &[Question]) -> Result<()> {
    let mut ids = HashSet::new();
    if questions.is_empty() || questions.len() > 4 {
        return invalid_answer();
    }
    for question in questions {
        if question.id.is_empty()
            || question.id.len() > 128
            || !ids.insert(&question.id)
            || question.question.trim().is_empty()
            || question.question.len() > 6000
            || question.header.len() > 200
            || question.options.len() > 10
            || (!question.allow_other && question.options.is_empty())
        {
            return invalid_answer();
        }
        let mut labels = HashSet::new();
        for option in &question.options {
            if option.label.trim().is_empty()
                || option.label.len() > 300
                || option.description.len() > 2000
                || !labels.insert(&option.label)
            {
                return invalid_answer();
            }
        }
    }
    Ok(())
}

pub(crate) fn resolved(request_id: &str, reason: &str) -> Value {
    json!({"method":"serverRequest/resolved","params":{"requestId":request_id,"reason":reason}})
}

pub(crate) fn json_line(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> NativeUserInputRequest {
        NativeUserInputRequest {
            native_id: json!(7),
            item_id: "tool-1".into(),
            input: Value::Null,
            is_blocking: true,
            questions: vec![Question {
                id: "project".into(),
                header: "Project".into(),
                question: "Which project?".into(),
                options: vec![QuestionOption {
                    label: "Alpha".into(),
                    description: "Relevant work".into(),
                }],
                multi_select: false,
                allow_other: true,
            }],
        }
    }

    #[test]
    fn user_input_answers_are_complete_execution_scoped_and_idempotent() {
        let mut pending = PendingQuestions::new(Some("execution-1"), true);
        let wire = pending.register(request(), "thread", "turn").unwrap();
        let mut answer = UserInputResponse {
            request_id: wire["id"].as_str().unwrap().into(),
            execution_id: "execution-2".into(),
            answers: BTreeMap::from([(
                "project".into(),
                UserInputAnswer {
                    answers: vec!["Custom project".into()],
                },
            )]),
        };
        assert!(pending.validate_response(&answer).is_err());
        assert!(pending.stale_response(&answer).is_none());
        answer.execution_id = "execution-1".into();
        assert!(pending.validate_response(&answer).unwrap().is_some());
        pending.answered(answer.clone());
        assert!(pending.validate_response(&answer).unwrap().is_none());
        answer.answers.get_mut("project").unwrap().answers = vec!["Alpha".into()];
        assert!(pending.validate_response(&answer).is_err());
    }

    #[test]
    fn user_input_cancel_and_native_id_reuse_cannot_answer_new_request() {
        let mut pending = PendingQuestions::new(Some("execution"), true);
        let first = pending.register(request(), "thread", "turn").unwrap();
        assert!(pending.cancel_native(&json!(7)).is_some());
        let second = pending.register(request(), "thread", "turn").unwrap();
        assert_ne!(first["id"], second["id"]);
        let answer = UserInputResponse {
            request_id: first["id"].as_str().unwrap().into(),
            execution_id: "execution".into(),
            answers: BTreeMap::new(),
        };
        assert!(pending.validate_response(&answer).is_err());
        assert_eq!(
            pending.stale_response(&answer).unwrap()["params"]["reason"],
            "cancelled"
        );
        assert_eq!(pending.cancel_all().len(), 1);
    }

    #[test]
    fn user_input_rejects_missing_extra_and_invalid_answers() {
        let mut pending = PendingQuestions::new(Some("execution"), true);
        let mut native = request();
        native.questions[0].allow_other = false;
        let wire = pending.register(native, "thread", "turn").unwrap();
        let mut answer = UserInputResponse {
            request_id: wire["id"].as_str().unwrap().into(),
            execution_id: "execution".into(),
            answers: BTreeMap::new(),
        };
        assert!(pending.validate_response(&answer).is_err());
        answer.answers.insert(
            "project".into(),
            UserInputAnswer {
                answers: vec!["Other".into()],
            },
        );
        assert!(pending.validate_response(&answer).is_err());
        answer.answers.get_mut("project").unwrap().answers = vec!["Alpha".into(), "Alpha".into()];
        assert!(pending.validate_response(&answer).is_err());
        answer.answers.get_mut("project").unwrap().answers = vec!["Alpha".into()];
        assert!(pending.validate_response(&answer).is_ok());
        answer.answers.insert(
            "extra".into(),
            UserInputAnswer {
                answers: vec!["Alpha".into()],
            },
        );
        assert!(pending.validate_response(&answer).is_err());
    }
}

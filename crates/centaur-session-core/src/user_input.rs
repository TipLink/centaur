//! Harness-neutral questions and answers carried by durable sessions.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestion {
    pub id: String,
    #[serde(default)]
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<UserInputOption>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default = "default_allow_other")]
    pub allow_other: bool,
}

fn default_allow_other() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserInputOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserInputAnswer {
    pub answers: Vec<String>,
}

pub type UserInputAnswers = BTreeMap<String, UserInputAnswer>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionUserInputRequest {
    pub request_id: String,
    pub execution_id: String,
    pub requester_user_id: Option<String>,
    pub questions: Vec<UserInputQuestion>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answers: Option<UserInputAnswers>,
}

pub fn validate_user_input_questions(questions: &[UserInputQuestion]) -> Result<(), String> {
    if questions.is_empty() || questions.len() > 4 {
        return Err("a request must contain between one and four questions".to_owned());
    }
    let mut ids = BTreeSet::new();
    for question in questions {
        if question.id.is_empty() || question.id.len() > 128 || !ids.insert(&question.id) {
            return Err("question ids must be unique and between 1 and 128 bytes".to_owned());
        }
        if question.question.trim().is_empty()
            || question.question.len() > 6000
            || question.header.len() > 200
            || question.options.len() > 10
        {
            return Err("question text or options exceed supported limits".to_owned());
        }
        let mut labels = BTreeSet::new();
        for option in &question.options {
            if option.label.trim().is_empty()
                || option.label.len() > 300
                || option.description.len() > 2000
                || !labels.insert(&option.label)
            {
                return Err(
                    "option labels must be nonempty and unique within a question".to_owned(),
                );
            }
        }
        if !question.allow_other && question.options.is_empty() {
            return Err("a question needs options or free-text input".to_owned());
        }
    }
    Ok(())
}

pub fn validate_user_input_answers(
    questions: &[UserInputQuestion],
    answers: &UserInputAnswers,
) -> Result<(), String> {
    if answers.len() != questions.len() {
        return Err("answer every question exactly once".to_owned());
    }
    for question in questions {
        let answer = answers
            .get(&question.id)
            .ok_or_else(|| "answer every question exactly once".to_owned())?;
        if answer.answers.is_empty()
            || answer.answers.len() > 16
            || (!question.multi_select && answer.answers.len() != 1)
        {
            return Err("invalid number of answers for question".to_owned());
        }
        let mut unique = BTreeSet::new();
        for value in &answer.answers {
            if value.trim().is_empty() || value.len() > 16000 || !unique.insert(value) {
                return Err("answers must be nonempty, bounded, and unique".to_owned());
            }
            if !question.allow_other
                && !question.options.iter().any(|option| &option.label == value)
            {
                return Err("answer is not one of the available options".to_owned());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn answers_preserve_free_text_and_enforce_question_cardinality() {
        let questions: Vec<UserInputQuestion> = serde_json::from_value(json!([
            {"id":"project","question":"Project?","options":[{"label":"A"},{"label":"B"}]},
            {"id":"scope","question":"Scope?","multiSelect":true,"allowOther":false,
             "options":[{"label":"API"},{"label":"UI"}]}
        ]))
        .unwrap();
        let answers: UserInputAnswers = serde_json::from_value(json!({
            "project":{"answers":["A different project"]},
            "scope":{"answers":["API","UI"]}
        }))
        .unwrap();
        assert!(validate_user_input_questions(&questions).is_ok());
        assert!(validate_user_input_answers(&questions, &answers).is_ok());
        let mut invalid = answers.clone();
        invalid.get_mut("scope").unwrap().answers = vec!["unlisted".to_owned()];
        assert!(validate_user_input_answers(&questions, &invalid).is_err());
        invalid = answers.clone();
        invalid
            .get_mut("project")
            .unwrap()
            .answers
            .push("A".to_owned());
        assert!(validate_user_input_answers(&questions, &invalid).is_err());
        invalid = answers;
        invalid.remove("project");
        assert!(validate_user_input_answers(&questions, &invalid).is_err());
    }

    #[test]
    fn duplicate_question_ids_and_option_labels_are_rejected() {
        let mut questions: Vec<UserInputQuestion> = serde_json::from_value(json!([
            {"id":"project","question":"Project?","options":[{"label":"A"},{"label":"A"}]}
        ]))
        .unwrap();
        assert!(validate_user_input_questions(&questions).is_err());
        questions[0].options.pop();
        questions.push(questions[0].clone());
        assert!(validate_user_input_questions(&questions).is_err());
    }
}

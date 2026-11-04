use centaur_session_core::HarnessType;
use serde_json::{Value, json};

/// Prompt generation is an opt-in capability of the current caller, not a
/// property of replayed user content. Only Claude implements native suggestions.
pub(super) fn input_lines_with_prompt_suggestions_support(
    input_lines: Vec<String>,
    harness_type: &HarnessType,
    metadata: Option<&Value>,
) -> Vec<String> {
    let enabled = *harness_type == HarnessType::ClaudeCode
        && metadata.is_some_and(|metadata| {
            metadata
                .get("supports_prompt_suggestions")
                .and_then(Value::as_bool)
                == Some(true)
                && metadata
                    .get("user_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.trim().is_empty())
        });
    input_lines
        .into_iter()
        .map(|line| {
            let Ok(Value::Object(mut value)) = serde_json::from_str::<Value>(&line) else {
                return line;
            };
            if value.get("type").and_then(Value::as_str) != Some("user") {
                return line;
            }
            let trace = value.entry("trace_metadata").or_insert_with(|| json!({}));
            if !trace.is_object() {
                *trace = json!({});
            }
            trace["supports_prompt_suggestions"] = json!(enabled);
            Value::Object(value).to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_capability(harness_type: &HarnessType, metadata: Option<&Value>) -> Value {
        let lines = input_lines_with_prompt_suggestions_support(
            vec![
                json!({
                    "type": "user",
                    "trace_metadata": {
                        "supports_prompt_suggestions": true,
                        "request_id": "preserved-request",
                    },
                    "message": {"role": "user", "content": []},
                })
                .to_string(),
            ],
            harness_type,
            metadata,
        );
        serde_json::from_str(&lines[0]).unwrap()
    }

    #[test]
    fn embedded_opt_in_cannot_enable_suggestions_without_the_current_requester() {
        for metadata in [
            None,
            Some(json!({"user_id": "U-requester"})),
            Some(json!({"supports_prompt_suggestions": true})),
            Some(json!({"supports_prompt_suggestions": true, "user_id": " "})),
            Some(json!({"supports_prompt_suggestions": false, "user_id": "U-requester"})),
            Some(json!({"supports_prompt_suggestions": "true", "user_id": "U-requester"})),
        ] {
            let line = with_capability(&HarnessType::ClaudeCode, metadata.as_ref());
            assert_eq!(line["trace_metadata"]["supports_prompt_suggestions"], false);
            assert_eq!(line["trace_metadata"]["request_id"], "preserved-request");
        }
    }

    #[test]
    fn caller_opt_in_applies_only_to_the_authoritative_claude_session() {
        let metadata = json!({"supports_prompt_suggestions": true, "user_id": "U-requester"});
        for harness_type in [
            HarnessType::Codex,
            HarnessType::Amp,
            HarnessType::Nanocodex,
            HarnessType::Hermes,
            HarnessType::Pi,
            HarnessType::Omp,
        ] {
            let line = with_capability(&harness_type, Some(&metadata));
            assert_eq!(line["trace_metadata"]["supports_prompt_suggestions"], false);
        }
        let line = with_capability(&HarnessType::ClaudeCode, Some(&metadata));
        assert_eq!(line["trace_metadata"]["supports_prompt_suggestions"], true);
    }
}

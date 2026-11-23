use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

use thiserror::Error;

use crate::traits::HarnessKind;

pub type Result<T> = std::result::Result<T, HarnessServerError>;

#[derive(Debug, Error)]
pub enum HarnessServerError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON error: {}", json_error_diagnostic(.0))]
    Json(#[from] serde_json::Error),
    #[error("Codex App Server protocol conversion failed: {0}")]
    Protocol(String),
    #[error("invalid request params: {}", json_error_diagnostic(.source))]
    InvalidParams {
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid blocks-mode input: {message}")]
    InvalidBlocksInput { message: String },
    #[error("required environment variable {name} is not set")]
    MissingEnvironment { name: &'static str },
    #[error("Nanocodex error: {0}")]
    Nanocodex(String),
    #[error("unknown threadId {thread_id}")]
    UnknownThread { thread_id: String },
    #[error("failed to spawn harness command in {cwd}: {source}")]
    SpawnHarness {
        cwd: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("harness stdin was not piped")]
    HarnessStdinUnavailable,
    #[error("harness stdout was not piped")]
    HarnessStdoutUnavailable,
    #[error("harness stderr was not piped")]
    HarnessStderrUnavailable,
    #[error("{kind:?} exited with status {status}{stderr}")]
    HarnessExited {
        kind: HarnessKind,
        status: ExitStatus,
        stderr: String,
    },
    #[error("Hermes exited with status {status}")]
    HermesExited { status: ExitStatus },
    #[error("{kind:?} did not finish its background work or follow-up before the deadline")]
    BackgroundFollowUpStalled { kind: HarnessKind },
    #[error("{message}")]
    UnknownModel { message: String },
    #[error("{kind:?} turn interrupted")]
    TurnInterrupted { kind: HarnessKind },
    #[error("failed to spawn {bin} app-server: {source}")]
    SpawnCodex {
        bin: String,
        #[source]
        source: io::Error,
    },
    #[error("Codex stdin was not piped")]
    CodexStdinUnavailable,
    #[error("Codex stdout was not piped")]
    CodexStdoutUnavailable,
    #[error("Codex stderr was not piped")]
    CodexStderrUnavailable,
    #[error("Codex app-server transport disconnected: {0}")]
    CodexTransport(#[source] io::Error),
    #[error("codex app-server exited with status {status}")]
    CodexExited { status: ExitStatus },
}

/// Longest error text sent to a client, in characters.
const MAX_CLIENT_MESSAGE_CHARS: usize = 512;
/// How much of a longer message's end is kept: an error often names its cause
/// last (an exit status, what a command expected).
const CLIENT_MESSAGE_TAIL_CHARS: usize = 96;
const TRUNCATION_MARK: &str = " ... (truncated) ... ";

impl HarnessServerError {
    /// The error text for clients: the wire, the turn's failure and telemetry.
    pub(crate) fn client_message(&self) -> String {
        bounded_message(&self.to_string())
    }
}

/// What a JSON error may say outside this process: its kind and position.
/// `serde_json`'s own text quotes the value it rejected (`invalid type: string
/// "<the whole value>", expected ...`), and that value is harness output or
/// user input: a prompt, a file, a secret. No character of it is copied here,
/// so nothing downstream (the wire, the stored turn error, telemetry, the
/// reply shown to the user) can carry it, however the text is cut later.
pub(crate) fn json_error_diagnostic(error: &serde_json::Error) -> String {
    use serde_json::error::Category;

    let kind = match error.classify() {
        Category::Io => "I/O failure",
        Category::Syntax => "invalid syntax",
        Category::Data => "invalid data",
        Category::Eof => "unexpected end of input",
    };
    // A value that was not parsed from text has no position.
    if error.line() == 0 {
        kind.to_string()
    } else {
        format!("{kind} at line {}, column {}", error.line(), error.column())
    }
}

/// Bounds an error message on its way to a client. An error can quote what it
/// failed on (a harness's stderr, the text of its own error result), which
/// would otherwise put that whole payload in the turn's error, and from there
/// in the execution record and in front of the user.
pub(crate) fn bounded_message(message: &str) -> String {
    if message.chars().nth(MAX_CLIENT_MESSAGE_CHARS).is_none() {
        return message.to_string();
    }
    let head_chars = MAX_CLIENT_MESSAGE_CHARS - CLIENT_MESSAGE_TAIL_CHARS - TRUNCATION_MARK.len();
    let mut boundaries = message.char_indices().map(|(index, _)| index);
    let head_end = boundaries.nth(head_chars).unwrap_or(message.len());
    let tail_start = boundaries
        .nth_back(CLIENT_MESSAGE_TAIL_CHARS - 1)
        .unwrap_or(head_end);
    format!(
        "{}{TRUNCATION_MARK}{}",
        &message[..head_end],
        &message[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_within_the_bound_are_unchanged() {
        for message in [
            String::new(),
            "boom".to_string(),
            "é".repeat(MAX_CLIENT_MESSAGE_CHARS),
        ] {
            assert_eq!(bounded_message(&message), message);
        }
    }

    #[test]
    fn long_messages_keep_their_start_and_end_on_character_boundaries() {
        // Multi-byte characters put every cut inside a character if cuts
        // were made on bytes.
        let message = format!("start {} end", "日本語🙂".repeat(5_000));
        let bounded = bounded_message(&message);

        assert_eq!(bounded.chars().count(), MAX_CLIENT_MESSAGE_CHARS);
        assert!(bounded.starts_with("start 日本語🙂"));
        assert!(bounded.contains(TRUNCATION_MARK));
        assert!(bounded.ends_with("日本語🙂 end"));
        assert_eq!(
            bounded_message(&"x".repeat(MAX_CLIENT_MESSAGE_CHARS + 1))
                .chars()
                .count(),
            MAX_CLIENT_MESSAGE_CHARS
        );
    }

    /// Markers at both ends of a rejected string: a cut that keeps the start
    /// or the end of an error's text would keep one of them.
    const START_MARKER: &str = "START-SECRET-7f3a";
    const END_MARKER: &str = "END-SECRET-9c1e";

    fn rejected_payload() -> String {
        format!(
            "{START_MARKER}<task-notification>{}</task-notification>{END_MARKER}",
            "internal ".repeat(2_500)
        )
    }

    #[test]
    fn a_parse_failure_reports_no_character_of_the_rejected_value() {
        let line =
            serde_json::json!({"type": "assistant", "message": rejected_payload()}).to_string();
        let source = serde_json::from_str::<crate::anthropic::AnthropicStreamEvent>(&line)
            .expect_err("a string is not a message");
        // What serde_json would have said, and what used to reach the client.
        assert!(source.to_string().contains(START_MARKER));
        assert!(source.to_string().contains(END_MARKER));

        let error = HarnessServerError::from(source);
        // An event's body is checked after serde has buffered it to find the
        // `type` tag, so this error has no position to report.
        assert_eq!(error.to_string(), "JSON error: invalid data");
        assert_eq!(error.client_message(), "JSON error: invalid data");
        for text in [error.to_string(), error.client_message()] {
            for leaked in [
                START_MARKER,
                END_MARKER,
                "task-notification",
                "internal",
                "\"",
            ] {
                assert!(!text.contains(leaked), "{leaked:?} in {text}");
            }
        }
    }

    #[test]
    fn json_diagnostics_name_the_kind_and_position_only() {
        let diagnostic = |text: &str| {
            json_error_diagnostic(&serde_json::from_str::<serde_json::Value>(text).unwrap_err())
        };
        assert_eq!(
            diagnostic("{\"secret\": tru"),
            "unexpected end of input at line 1, column 14"
        );
        assert_eq!(
            diagnostic("{\"a\": 1}\nsecret"),
            "invalid syntax at line 2, column 1"
        );
        assert_eq!(
            json_error_diagnostic(&serde_json::from_str::<Vec<u8>>("\n  \"secret\"").unwrap_err()),
            "invalid data at line 2, column 10"
        );
        // A value converted without text to point into has no position.
        let params = HarnessServerError::InvalidParams {
            source: serde_json::from_value::<Vec<u8>>(serde_json::json!({"secret": "value"}))
                .unwrap_err(),
        };
        assert_eq!(params.to_string(), "invalid request params: invalid data");
    }
}

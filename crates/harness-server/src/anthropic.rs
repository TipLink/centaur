use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

use crate::{
    NormalizedContent, NormalizedEvent, NormalizedTokenUsage, NormalizedToolResult, Result,
    stable_id,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicStreamEvent {
    System {
        subtype: Option<String>,
        session_id: Option<String>,
        #[serde(flatten)]
        task: AnthropicSystemTask,
    },
    Assistant {
        #[serde(default)]
        is_partial: bool,
        message: AnthropicMessage,
        #[serde(default)]
        parent_tool_use_id: Option<String>,
    },
    User {
        message: AnthropicMessage,
        tool_use_result: Option<Value>,
        #[serde(default)]
        parent_tool_use_id: Option<String>,
        /// Claude Code (`--replay-user-messages`) echoes each user message it
        /// takes in, keeping the `uuid` it was sent with.
        #[serde(default)]
        uuid: Option<String>,
        #[serde(default, rename = "isReplay")]
        is_replay: bool,
    },
    StreamEvent {
        event: AnthropicRawStreamEvent,
        #[serde(default)]
        parent_tool_use_id: Option<String>,
    },
    Result {
        subtype: Option<String>,
        result: Option<String>,
        #[serde(default)]
        is_error: bool,
        error: Option<Value>,
        message: Option<String>,
        usage: Option<Value>,
    },
    Error {
        error: Option<Value>,
        message: Option<String>,
        result: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

/// The two top-level fields that classify a line without parsing its body.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TopLevelEnvelope {
    pub kind: Option<String>,
    /// Present and not `null`.
    pub has_parent_tool_use_id: bool,
}

/// Scans only the top-level keys of a JSON object without recursion or
/// allocating a `Value` tree, so deeply nested or unreadable bodies on
/// non-matching lines (such as subagent sidechains) never trip `serde_json`'s
/// recursion limit or typed validation. Returns `None` if the line is not a
/// syntactically balanced top-level JSON object, letting callers fall through
/// to full `serde_json` parsing for error reporting.
pub(crate) fn scan_top_level_envelope(line: &str) -> Option<TopLevelEnvelope> {
    let bytes = line.as_bytes();
    let mut i = skip_json_ws(bytes, 0);
    if bytes.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    let mut envelope = TopLevelEnvelope::default();
    let mut first = true;
    loop {
        i = skip_json_ws(bytes, i);
        if bytes.get(i) == Some(&b'}') {
            i = skip_json_ws(bytes, i + 1);
            return (i == bytes.len()).then_some(envelope);
        }
        if !first {
            if bytes.get(i) != Some(&b',') {
                return None;
            }
            i = skip_json_ws(bytes, i + 1);
        }
        first = false;

        let key_start = i;
        let key_end = scan_json_string(bytes, key_start)?;
        i = skip_json_ws(bytes, key_end);
        if bytes.get(i) != Some(&b':') {
            return None;
        }
        let val_start = skip_json_ws(bytes, i + 1);
        let val_end = scan_json_value(line, val_start)?;
        i = val_end;

        let key_slice = &line[key_start..key_end];
        let val_slice = &line[val_start..val_end];
        let matched_key = if key_slice == "\"type\"" {
            Some("type")
        } else if key_slice == "\"parent_tool_use_id\"" {
            Some("parent_tool_use_id")
        } else if key_slice.contains('\\') {
            match serde_json::from_str::<String>(key_slice).ok()?.as_str() {
                "type" => Some("type"),
                "parent_tool_use_id" => Some("parent_tool_use_id"),
                _ => None,
            }
        } else {
            None
        };
        match matched_key {
            Some("type") => {
                envelope.kind = if val_slice.starts_with('"') {
                    serde_json::from_str::<String>(val_slice).ok()
                } else {
                    None
                };
            }
            Some("parent_tool_use_id") => {
                envelope.has_parent_tool_use_id = val_slice != "null";
            }
            _ => {}
        }
    }
}

fn skip_json_ws(bytes: &[u8], mut i: usize) -> usize {
    while let Some(&b) = bytes.get(i) {
        if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
            i += 1;
        } else {
            break;
        }
    }
    i
}

fn scan_json_string(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut i = start + 1;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'"' => return Some(i + 1),
            b'\\' => {
                if i + 1 >= bytes.len() {
                    return None;
                }
                i += 2;
            }
            0x00..=0x1f => return None,
            _ => i += 1,
        }
    }
    None
}

fn scan_json_value(line: &str, start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    match *bytes.get(start)? {
        b'"' => scan_json_string(bytes, start),
        b'{' | b'[' => scan_json_container(bytes, start),
        _ => {
            let mut i = start;
            while let Some(&b) = bytes.get(i) {
                if matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b',' | b'}' | b']') {
                    break;
                }
                i += 1;
            }
            if i == start {
                return None;
            }
            let token = &line[start..i];
            let valid = matches!(token, "null" | "true" | "false")
                || serde_json::from_str::<serde_json::Number>(token).is_ok();
            valid.then_some(i)
        }
    }
}

fn scan_json_container(bytes: &[u8], start: usize) -> Option<usize> {
    let opener = *bytes.get(start)?;
    let mut stack = vec![if opener == b'{' { b'}' } else { b']' }];
    let mut i = start + 1;
    while !stack.is_empty() {
        let &b = bytes.get(i)?;
        match b {
            b'"' => i = scan_json_string(bytes, i)?,
            b'{' => {
                stack.push(b'}');
                i += 1;
            }
            b'[' => {
                stack.push(b']');
                i += 1;
            }
            b'}' | b']' => {
                if stack.pop() != Some(b) {
                    return None;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    Some(i)
}

impl AnthropicStreamEvent {
    pub fn parse_json_line(line: &str) -> Result<Self> {
        Ok(serde_json::from_str(line)?)
    }

    /// Parses a line of the main chain, and reads a subagent sidechain's no
    /// further than its envelope: it becomes `Unknown`, which every consumer
    /// ignores. A sidechain's messages never reach the turn (its report
    /// arrives as the main chain's Task tool result), so a body this parser
    /// cannot read — including deeply nested tool input — must not fail the
    /// parent turn. Only the event types that carry `parent_tool_use_id` (see
    /// `Self::parent_tool_use_id`) are skipped. Every main-chain line is still
    /// parsed and validated in full.
    pub fn parse_main_chain_json_line(line: &str) -> Result<Self> {
        if let Some(envelope) = scan_top_level_envelope(line) {
            let owned_by_a_chain = matches!(
                envelope.kind.as_deref(),
                Some("assistant" | "user" | "stream_event")
            );
            if owned_by_a_chain && envelope.has_parent_tool_use_id {
                return Ok(Self::Unknown);
            }
        }
        Self::parse_json_line(line)
    }

    /// The authoritative end-of-message stop reason, carried by the raw
    /// `message_delta` stream event (`tool_use`, `end_turn`, ...). The per-block
    /// `assistant` events leave `stop_reason` null, so this is the only in-stream
    /// signal of whether a message is interim commentary or the final answer.
    pub fn message_stop_reason(&self) -> Option<&str> {
        match self {
            Self::StreamEvent {
                event:
                    AnthropicRawStreamEvent::MessageDelta {
                        delta: Some(delta), ..
                    },
                ..
            } => delta.stop_reason.as_deref(),
            _ => None,
        }
    }

    /// The Task tool-use id owning this event when it belongs to a subagent
    /// sidechain. Sidechain messages stop with their own `end_turn` while the
    /// parent turn keeps running, so they must never settle the turn.
    pub fn parent_tool_use_id(&self) -> Option<&str> {
        match self {
            Self::Assistant {
                parent_tool_use_id, ..
            }
            | Self::User {
                parent_tool_use_id, ..
            }
            | Self::StreamEvent {
                parent_tool_use_id, ..
            } => parent_tool_use_id.as_deref(),
            _ => None,
        }
    }

    pub fn is_sidechain(&self) -> bool {
        self.parent_tool_use_id().is_some()
    }

    pub fn token_usage(&self) -> Option<NormalizedTokenUsage> {
        match self {
            Self::Assistant { message, .. } => {
                token_usage_from_value(message.usage.as_ref(), message.model.clone())
            }
            Self::Result { usage, .. } => token_usage_from_value(usage.as_ref(), None),
            _ => None,
        }
    }
}

/// Background-task fields Claude Code puts on `system` lifecycle events
/// (`task_started`, `task_notification`, `background_tasks_changed`, ...).
/// Every `system` line is flattened into this, so each field is lenient: an
/// unexpected type reads as absent instead of failing the line (and turn).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AnthropicSystemTask {
    #[serde(default, deserialize_with = "lenient")]
    pub task_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub tool_use_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub task_type: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub subagent_type: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub status: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub summary: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub is_backgrounded: bool,
    #[serde(default, deserialize_with = "lenient")]
    pub owned_by_subagent: bool,
    /// `background_tasks_changed`: every live background task.
    #[serde(default, deserialize_with = "lenient")]
    pub tasks: Option<Vec<AnthropicBackgroundTask>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AnthropicBackgroundTask {
    #[serde(default, deserialize_with = "lenient")]
    pub task_id: Option<String>,
}

fn lenient<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    Ok(T::deserialize(Value::deserialize(deserializer)?).unwrap_or_default())
}

impl AnthropicSystemTask {
    /// Background subagents (Agent tool with `run_in_background`) launched by
    /// the main chain. Their completion makes Claude Code run a follow-up
    /// turn on its own after the turn that launched them has already ended.
    pub fn is_main_chain_agent(&self) -> bool {
        self.task_type.as_deref() == Some("local_agent") && !self.owned_by_subagent
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicRawStreamEvent {
    MessageStart {
        message: AnthropicMessage,
    },
    ContentBlockStart {
        index: usize,
        content_block: AnthropicContentBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: AnthropicContentDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageStop,
    MessageDelta {
        #[serde(default)]
        delta: Option<AnthropicMessageDeltaBody>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicMessageDeltaBody {
    #[serde(default)]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicContentDelta {
    TextDelta {
        #[serde(default)]
        text: String,
    },
    ThinkingDelta {
        #[serde(default)]
        thinking: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone)]
enum StreamBlockKind {
    Text,
    Reasoning,
    Other,
}

#[derive(Debug, Default)]
pub struct AnthropicEventNormalizer {
    current_message_item_id: Option<String>,
    content_blocks: HashMap<usize, StreamBlockKind>,
}

impl AnthropicEventNormalizer {
    pub fn normalize(&mut self, event: AnthropicStreamEvent) -> NormalizedEvent {
        match event {
            AnthropicStreamEvent::StreamEvent { event, .. } => self.normalize_stream_event(event),
            event => self.normalize_message_event(event),
        }
    }

    fn normalize_stream_event(&mut self, event: AnthropicRawStreamEvent) -> NormalizedEvent {
        match event {
            AnthropicRawStreamEvent::MessageStart { message } => {
                self.current_message_item_id = Some(assistant_item_id(message.id.as_deref()));
                self.content_blocks.clear();
                NormalizedEvent::Ignored
            }
            AnthropicRawStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                self.content_blocks
                    .insert(index, stream_block_kind(content_block));
                NormalizedEvent::Ignored
            }
            AnthropicRawStreamEvent::ContentBlockDelta { index, delta } => {
                let message_id = self
                    .current_message_item_id
                    .clone()
                    .unwrap_or_else(|| assistant_item_id(None));
                match delta {
                    AnthropicContentDelta::TextDelta { text } => match self
                        .content_blocks
                        .get(&index)
                    {
                        Some(StreamBlockKind::Reasoning) => NormalizedEvent::ReasoningTextDelta {
                            item_id: reasoning_item_id(&message_id, index),
                            delta: text,
                        },
                        _ => NormalizedEvent::AgentTextDelta {
                            item_id: message_id,
                            delta: text,
                        },
                    },
                    AnthropicContentDelta::ThinkingDelta { thinking } => {
                        NormalizedEvent::ReasoningTextDelta {
                            item_id: reasoning_item_id(&message_id, index),
                            delta: thinking,
                        }
                    }
                    AnthropicContentDelta::Unknown => NormalizedEvent::Ignored,
                }
            }
            AnthropicRawStreamEvent::ContentBlockStop { index } => {
                self.content_blocks.remove(&index);
                NormalizedEvent::Ignored
            }
            AnthropicRawStreamEvent::MessageStop => {
                self.current_message_item_id = None;
                self.content_blocks.clear();
                NormalizedEvent::Ignored
            }
            AnthropicRawStreamEvent::MessageDelta { .. } | AnthropicRawStreamEvent::Unknown => {
                NormalizedEvent::Ignored
            }
        }
    }

    fn normalize_message_event(&mut self, event: AnthropicStreamEvent) -> NormalizedEvent {
        match event {
            AnthropicStreamEvent::System {
                subtype,
                session_id,
                ..
            } => {
                if subtype.as_deref() == Some("init") {
                    NormalizedEvent::SessionStarted { session_id }
                } else {
                    NormalizedEvent::Ignored
                }
            }
            AnthropicStreamEvent::Assistant {
                is_partial,
                message,
                ..
            } => NormalizedEvent::AssistantMessage {
                partial: is_partial,
                stop_reason: message.stop_reason.clone(),
                content: message.into_normalized_content(),
            },
            AnthropicStreamEvent::User {
                message,
                tool_use_result,
                ..
            } => {
                let tool_use_result = tool_use_result.as_ref();
                let results: Vec<_> = message
                    .content
                    .into_iter()
                    .filter_map(|block| block.into_tool_result(tool_use_result))
                    .collect();
                // A user message without tool results (plain text, a background
                // task's `<task-notification>`) is not a tool boundary: reported
                // as one, it would settle pending agent text as commentary.
                if results.is_empty() {
                    NormalizedEvent::Ignored
                } else {
                    NormalizedEvent::ToolResults(results)
                }
            }
            AnthropicStreamEvent::Result {
                subtype,
                result,
                is_error,
                error,
                message,
                usage: _,
            } => NormalizedEvent::Result {
                error: result_error_text(subtype, is_error, error, message, result),
            },
            AnthropicStreamEvent::Error {
                error,
                message,
                result,
            } => NormalizedEvent::Error {
                message: event_error_text(error, message, result)
                    .unwrap_or_else(|| "harness error".to_string()),
            },
            AnthropicStreamEvent::StreamEvent { .. } | AnthropicStreamEvent::Unknown => {
                NormalizedEvent::Ignored
            }
        }
    }
}

impl From<AnthropicStreamEvent> for NormalizedEvent {
    fn from(event: AnthropicStreamEvent) -> Self {
        AnthropicEventNormalizer::default().normalize(event)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicMessage {
    pub id: Option<String>,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub usage: Option<Value>,
    #[serde(default, deserialize_with = "deserialize_message_content")]
    pub content: Vec<AnthropicContentBlock>,
}

/// Message `content` is a list of blocks, or the Messages API's shorthand of a
/// plain string for a single text block. Claude Code uses the shorthand for
/// user messages it writes itself, such as a background task's
/// `<task-notification>`, and echoes them under `--replay-user-messages`
/// (observed with 2.1.325). A string reads as one text block and `null` as no
/// blocks, instead of failing the line and with it the turn.
fn deserialize_message_content<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<AnthropicContentBlock>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ContentVisitor;

    impl<'de> serde::de::Visitor<'de> for ContentVisitor {
        type Value = Vec<AnthropicContentBlock>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a string, null or a sequence of content blocks")
        }

        fn visit_str<E>(self, text: &str) -> std::result::Result<Self::Value, E> {
            Ok(vec![AnthropicContentBlock::Text {
                text: text.to_string(),
            }])
        }

        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(self)
        }

        fn visit_seq<A>(self, blocks: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            Deserialize::deserialize(serde::de::value::SeqAccessDeserializer::new(blocks))
        }
    }

    deserializer.deserialize_any(ContentVisitor)
}

impl AnthropicMessage {
    /// Claude Code attributes text it wrote without calling the model (an API
    /// error notice, "No response requested.") to this model name.
    pub fn is_synthetic(&self) -> bool {
        self.model.as_deref() == Some("<synthetic>")
    }

    fn into_normalized_content(self) -> Vec<NormalizedContent> {
        let message_id = assistant_item_id(self.id.as_deref());
        self.content
            .into_iter()
            .enumerate()
            .filter_map(|(index, block)| block.into_normalized_content(&message_id, index))
            .collect()
    }
}

fn assistant_item_id(raw_id: Option<&str>) -> String {
    stable_id(raw_id.unwrap_or("assistant"), "msg")
}

fn reasoning_item_id(message_id: &str, index: usize) -> String {
    format!("{message_id}-reasoning-{index}")
}

fn stream_block_kind(block: AnthropicContentBlock) -> StreamBlockKind {
    match block {
        AnthropicContentBlock::Text { .. } => StreamBlockKind::Text,
        AnthropicContentBlock::Thinking { .. } | AnthropicContentBlock::Reasoning { .. } => {
            StreamBlockKind::Reasoning
        }
        AnthropicContentBlock::ToolUse { .. }
        | AnthropicContentBlock::ToolResult { .. }
        | AnthropicContentBlock::Unknown => StreamBlockKind::Other,
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicContentBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    Thinking {
        thinking: Option<String>,
        text: Option<String>,
    },
    Reasoning {
        thinking: Option<String>,
        text: Option<String>,
    },
    ToolUse {
        id: Option<String>,
        name: Option<String>,
        input: Option<Value>,
    },
    ToolResult {
        tool_use_id: Option<String>,
        content: Option<Value>,
        #[serde(default)]
        is_error: bool,
    },
    #[serde(other)]
    Unknown,
}

impl AnthropicContentBlock {
    fn into_normalized_content(self, message_id: &str, index: usize) -> Option<NormalizedContent> {
        match self {
            Self::Text { text } => Some(NormalizedContent::AgentText {
                item_id: message_id.to_string(),
                text,
            }),
            Self::Thinking { thinking, text } | Self::Reasoning { thinking, text } => {
                Some(NormalizedContent::ReasoningText {
                    item_id: format!("{message_id}-reasoning-{index}"),
                    text: thinking.or(text).unwrap_or_default(),
                })
            }
            Self::ToolUse { id, name, input } => Some(NormalizedContent::ToolUse {
                raw_id: id.unwrap_or_else(|| "tool".to_string()),
                tool: name.unwrap_or_else(|| "tool".to_string()),
                arguments: input.unwrap_or_else(|| serde_json::json!({})),
            }),
            Self::ToolResult { .. } | Self::Unknown => None,
        }
    }

    fn into_tool_result(self, runtime_result: Option<&Value>) -> Option<NormalizedToolResult> {
        match self {
            Self::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let content_text = content_text(content.as_ref());
                let (content_text, parsed_exit_code) =
                    parse_json_tool_result(&content_text).unwrap_or((content_text, None));
                Some(NormalizedToolResult {
                    tool_use_id: tool_use_id.unwrap_or_else(|| "tool".to_string()),
                    content: runtime_output_text(runtime_result).unwrap_or(content_text.clone()),
                    is_error,
                    exit_code: runtime_exit_code(runtime_result)
                        .or(parsed_exit_code)
                        .or_else(|| exit_code_from_prefix(&content_text)),
                })
            }
            _ => None,
        }
    }
}

fn result_error_text(
    subtype: Option<String>,
    is_error: bool,
    error: Option<Value>,
    message: Option<String>,
    result: Option<String>,
) -> Option<String> {
    let subtype_is_error = subtype
        .as_deref()
        .is_some_and(|subtype| !matches!(subtype, "" | "success"));
    if is_error || subtype_is_error {
        event_error_text(error, message, result)
            .or_else(|| Some("harness reported an error".to_string()))
    } else {
        None
    }
}

fn event_error_text(
    error: Option<Value>,
    message: Option<String>,
    result: Option<String>,
) -> Option<String> {
    error
        .and_then(|error| {
            error.as_str().map(str::to_string).or_else(|| {
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
        })
        .or(message)
        .or(result)
}

fn content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| item.as_str())
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn runtime_output_text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let stdout = value.get("stdout").and_then(Value::as_str);
    let stderr = value.get("stderr").and_then(Value::as_str);
    if stdout.is_some() || stderr.is_some() {
        Some(format!("{}{}", stdout.unwrap_or(""), stderr.unwrap_or("")))
    } else {
        None
    }
}

fn runtime_exit_code(value: Option<&Value>) -> Option<i32> {
    let value = value?;
    value
        .get("exit_code")
        .or_else(|| value.get("exitCode"))
        .and_then(Value::as_i64)
        .and_then(|code| i32::try_from(code).ok())
}

fn token_usage_from_value(
    value: Option<&Value>,
    model: Option<String>,
) -> Option<NormalizedTokenUsage> {
    let value = value?;
    if value.is_null() {
        return None;
    }
    let usage = NormalizedTokenUsage {
        model: model.or_else(|| token_string(value, &["model"])),
        input_tokens: token_count(value, &["input_tokens", "inputTokens", "inputTokenCount"]),
        output_tokens: token_count(
            value,
            &["output_tokens", "outputTokens", "outputTokenCount"],
        ),
        cache_creation_input_tokens: token_count(
            value,
            &[
                "cache_creation_input_tokens",
                "cacheCreationInputTokens",
                "cacheCreationTokens",
            ],
        ),
        cache_read_input_tokens: token_count(
            value,
            &[
                "cache_read_input_tokens",
                "cached_input_tokens",
                "cacheReadInputTokens",
                "cachedInputTokens",
            ],
        ),
        reasoning_output_tokens: token_count(
            value,
            &[
                "reasoning_output_tokens",
                "reasoning_tokens",
                "reasoningOutputTokens",
                "reasoningTokens",
            ],
        ),
        total_tokens: token_count(value, &["total_tokens", "totalTokens", "totalTokenCount"]),
    };
    usage.has_counts().then_some(usage)
}

fn token_count(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .find_map(value_as_i64)
}

fn token_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|value| *value >= 0)
}

fn parse_json_tool_result(content: &str) -> Option<(String, Option<i32>)> {
    let value: Value = serde_json::from_str(content).ok()?;
    let object = value.as_object()?;
    let output = object
        .get("output")
        .and_then(Value::as_str)
        .map(str::to_string)?;
    let exit_code = object
        .get("exitCode")
        .or_else(|| object.get("exit_code"))
        .and_then(Value::as_i64)
        .and_then(|code| i32::try_from(code).ok());
    Some((output, exit_code))
}

fn exit_code_from_prefix(content: &str) -> Option<i32> {
    let rest = content.strip_prefix("Exit code ")?;
    let code_text = rest.split_once('\n').map_or(rest, |(code, _)| code);
    code_text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn assistant_event_exposes_token_usage() {
        let event: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"assistant","message":{"model":"claude-fable-5","id":"msg_1","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":2,"cache_creation_input_tokens":3,"cache_read_input_tokens":5,"output_tokens":7}}}"#,
        )
        .expect("assistant event");
        let usage = event.token_usage().expect("usage");

        assert_eq!(usage.model.as_deref(), Some("claude-fable-5"));
        assert_eq!(usage.input_tokens, Some(2));
        assert_eq!(usage.cache_creation_input_tokens, Some(3));
        assert_eq!(usage.cache_read_input_tokens, Some(5));
        assert_eq!(usage.output_tokens, Some(7));
    }

    #[test]
    fn result_event_exposes_camel_case_token_usage() {
        let event: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"result","subtype":"success","usage":{"inputTokens":11,"cachedInputTokens":13,"outputTokens":17,"totalTokens":41}}"#,
        )
        .expect("result event");
        let usage = event.token_usage().expect("usage");

        assert_eq!(usage.input_tokens, Some(11));
        assert_eq!(usage.cache_read_input_tokens, Some(13));
        assert_eq!(usage.output_tokens, Some(17));
        assert_eq!(usage.total_tokens, Some(41));
    }

    fn user_message(content: Value) -> AnthropicMessage {
        match serde_json::from_value(json!({"type": "user", "message": content})) {
            Ok(AnthropicStreamEvent::User { message, .. }) => message,
            other => panic!("expected a user event, got {other:?}"),
        }
    }

    #[test]
    fn message_content_reads_blocks_a_plain_string_null_and_absence() {
        let blocks = user_message(json!({"content": [
            {"type": "text", "text": "hi"},
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"},
            {"type": "from_a_newer_cli"},
        ]}));
        assert!(matches!(
            blocks.content.as_slice(),
            [
                AnthropicContentBlock::Text { text },
                AnthropicContentBlock::ToolResult { .. },
                AnthropicContentBlock::Unknown,
            ] if text == "hi"
        ));

        let notification = "<task-notification>\n<task-id>a1</task-id>\n</task-notification>";
        let string = user_message(json!({"role": "user", "content": notification}));
        assert!(matches!(
            string.content.as_slice(),
            [AnthropicContentBlock::Text { text }] if text == notification
        ));

        assert!(user_message(json!({"content": null})).content.is_empty());
        assert!(user_message(json!({"role": "user"})).content.is_empty());
    }

    /// The line that failed a production turn: Claude Code 2.1.325 echoing a
    /// background task's notification under `--replay-user-messages`.
    #[test]
    fn replayed_task_notification_with_string_content_parses() {
        let event = AnthropicStreamEvent::parse_json_line(
            r#"{"type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>a1b2</task-id>\n<status>completed</status>\n</task-notification>"},"parent_tool_use_id":null,"session_id":"s1","uuid":"7d0e","isReplay":true}"#,
        )
        .expect("string content parses");
        assert!(matches!(
            event,
            AnthropicStreamEvent::User { is_replay: true, uuid: Some(uuid), .. } if uuid == "7d0e"
        ));
    }

    #[test]
    fn message_content_of_another_type_still_fails_the_line() {
        let error = AnthropicStreamEvent::parse_json_line(
            r#"{"type":"user","message":{"content":{"type":"text","text":"hi"}}}"#,
        )
        .expect_err("an object is not message content");
        // What reaches a client says nothing of the value. (An event's body
        // is checked after serde has buffered it to find the `type` tag, so
        // such an error has no position to report either.)
        assert_eq!(error.to_string(), "JSON error: invalid data");
        let crate::HarnessServerError::Json(source) = error else {
            panic!("expected a JSON error, got {error:?}");
        };
        assert!(
            source
                .to_string()
                .contains("expected a string, null or a sequence of content blocks"),
            "{source}"
        );
    }

    /// Bodies the typed parser rejects: a message that is a string, a content
    /// block that is a number, a stream event with no index.
    fn malformed_events(parent_tool_use_id: Option<Value>) -> Vec<String> {
        [
            json!({"type": "assistant", "message": "SECRET-BODY not a message"}),
            json!({"type": "user", "message": {"role": "user", "content": [7, "SECRET-BODY"]}}),
            json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": "SECRET-BODY"}}),
        ]
        .into_iter()
        .map(|mut event| {
            if let Some(parent) = &parent_tool_use_id {
                event["parent_tool_use_id"] = parent.clone();
            }
            event.to_string()
        })
        .collect()
    }

    #[test]
    fn sidechain_events_are_not_read_past_their_envelope() {
        // A subagent's event with a body this parser cannot read used to fail
        // the parent turn, although sidechain events are dropped anyway.
        for parent in [json!("toolu_task_1"), json!(""), json!({"id": 1})] {
            for line in malformed_events(Some(parent)) {
                assert!(
                    AnthropicStreamEvent::parse_json_line(&line).is_err(),
                    "the fixture must be unreadable in full: {line}"
                );
                let event = AnthropicStreamEvent::parse_main_chain_json_line(&line)
                    .unwrap_or_else(|error| panic!("{error}: {line}"));
                assert!(matches!(event, AnthropicStreamEvent::Unknown), "{line}");
                assert!(matches!(
                    NormalizedEvent::from(event),
                    NormalizedEvent::Ignored
                ));
            }
        }
        // A well-formed sidechain event is skipped the same way.
        let event = AnthropicStreamEvent::parse_main_chain_json_line(
            r#"{"type":"assistant","parent_tool_use_id":"toolu_task_1","message":{"id":"msg_s","stop_reason":"end_turn","content":[{"type":"text","text":"subagent"}]}}"#,
        )
        .expect("sidechain event");
        assert!(matches!(event, AnthropicStreamEvent::Unknown));

        // Valid JSON nested deeper than serde_json's recursion limit (128) on
        // a sidechain body must also be skipped without failing the line.
        let deep = format!("{}0{}", r#"{"a":"#.repeat(200), "}".repeat(200));
        let deep_sidechain = format!(
            r#"{{"type":"assistant","parent_tool_use_id":"toolu_task_1","message":{{"id":"msg_deep","content":[{{"type":"tool_use","id":"toolu_1","name":"Bash","input":{deep}}}]}}}}"#
        );
        assert!(
            AnthropicStreamEvent::parse_json_line(&deep_sidechain).is_err(),
            "deeply nested fixture should exceed serde_json recursion limit"
        );
        let event = AnthropicStreamEvent::parse_main_chain_json_line(&deep_sidechain)
            .expect("deeply nested sidechain event");
        assert!(matches!(event, AnthropicStreamEvent::Unknown));
    }

    #[test]
    fn main_chain_events_are_still_parsed_and_validated_in_full() {
        for parent in [None, Some(Value::Null)] {
            for line in malformed_events(parent) {
                let error = AnthropicStreamEvent::parse_main_chain_json_line(&line)
                    .expect_err("a malformed main-chain event fails the line");
                assert_eq!(error.to_string(), "JSON error: invalid data", "{line}");
            }
        }
        let event = AnthropicStreamEvent::parse_main_chain_json_line(
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"id":"msg_1","content":[{"type":"text","text":"main"}]}}"#,
        )
        .expect("main-chain event");
        assert!(matches!(event, AnthropicStreamEvent::Assistant { .. }));

        // `system` and `result` lines carry no chain of their own: a field of
        // that name on one does not hide it.
        let event = AnthropicStreamEvent::parse_main_chain_json_line(
            r#"{"type":"system","subtype":"init","session_id":"s1","parent_tool_use_id":"toolu_task_1"}"#,
        )
        .expect("system event");
        assert!(matches!(
            NormalizedEvent::from(event),
            NormalizedEvent::SessionStarted { session_id: Some(session_id) } if session_id == "s1"
        ));
        let event = AnthropicStreamEvent::parse_main_chain_json_line(
            r#"{"type":"result","subtype":"success","parent_tool_use_id":"toolu_task_1"}"#,
        )
        .expect("result event");
        assert!(matches!(event, AnthropicStreamEvent::Result { .. }));

        // A line that is not JSON at all fails without quoting itself.
        let error = AnthropicStreamEvent::parse_main_chain_json_line("SECRET-BODY {")
            .expect_err("not JSON");
        assert_eq!(
            error.to_string(),
            "JSON error: invalid syntax at line 1, column 1"
        );
    }

    #[test]
    fn user_messages_without_tool_results_are_ignored() {
        for content in [
            json!("<task-notification>done</task-notification>"),
            json!([{"type": "text", "text": "plain text"}]),
            json!([]),
            Value::Null,
        ] {
            let event = serde_json::from_value::<AnthropicStreamEvent>(
                json!({"type": "user", "message": {"role": "user", "content": content}}),
            )
            .expect("user event");
            assert!(matches!(
                NormalizedEvent::from(event),
                NormalizedEvent::Ignored
            ));
        }

        let event = serde_json::from_value::<AnthropicStreamEvent>(json!({
            "type": "user",
            "message": {"content": [
                {"type": "text", "text": "and a note"},
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"},
            ]},
        }))
        .expect("user event");
        assert!(matches!(
            NormalizedEvent::from(event),
            NormalizedEvent::ToolResults(results)
                if results.len() == 1 && results[0].tool_use_id == "toolu_1"
        ));
    }
}

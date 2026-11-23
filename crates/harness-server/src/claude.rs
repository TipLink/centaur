use std::collections::{HashMap, HashSet};
use std::env;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

use crate::protocol::UserInput;
use crate::user_input::{
    Answers, NativeUserInputRequest, QUESTION_GUIDANCE, Question, UserInputControl, json_line,
};
use crate::{
    HarnessKind, HarnessServer, NormalizedContent, NormalizedEvent, NormalizedToolResult, Result,
    ThreadState, TurnHold,
    anthropic::{AnthropicEventNormalizer, AnthropicRawStreamEvent, AnthropicStreamEvent},
    command_from_override, user_input_to_anthropic_content,
};

/// Effort levels Claude Code accepts for its `effortLevel` setting.
const CLAUDE_EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Defers agent text until the owning message's fate is known, so agentMessage
/// items can be emitted with an authoritative stop reason. Claude's per-block
/// `assistant` events leave `stop_reason` null, and downstream renderers treat
/// unphased messages as the final answer, so completing each text block as it
/// arrives makes every interim message render as an extra reply. Text is held
/// until a `message_delta` stop reason, a `tool_use` in the same message, a tool
/// result, a newer message, or the terminal `result` settles whether the text was
/// commentary (`tool_use`) or the final answer (`end_turn`). Flushing replays the
/// original delta chunks after an `AgentMessageStarted` carrying the stop reason,
/// so the item starts with the right phase and native chunking is preserved.
///
/// Background subagents, Monitor watches, and explicitly timed shell tasks outlive the
/// message that launched them: Claude Code emits that turn's `result`, then
/// runs a follow-up turn on its own once a task's `task_notification`
/// lands. While main-chain tasks are running — or a notification has not
/// yet been answered by a new main-chain message — the turn is held open:
/// terminal text flushes as commentary and the interim `result` is swallowed,
/// so the follow-up's answer is delivered by this turn instead of being
/// drained as stale output before the next one. Each background task is
/// surfaced as a tool item that starts on `task_started` and completes with
/// the task's summary on `task_notification`; a resumed agent starts again
/// and gets a new item.
///
/// Steers are held the same way. Claude Code takes a user message sent
/// mid-turn into its work at the next tool result, or, when none is left,
/// answers it in a follow-up turn of its own after the turn's `result`
/// (observed with 2.1.318). With `--replay-user-messages` it echoes each user
/// message as it takes it in, keeping the `uuid` it was sent with, so a
/// steer not yet echoed when `result` arrives is owed a follow-up, which this
/// turn waits for.
///
/// The turn's own prompt is tracked the same way. A resumed session first
/// settles what its previous process left behind (tool calls cut short, the
/// `<task-notification>` of each background task that died with it) and can
/// end that with a `result` of its own before it dequeues the prompt
/// (observed with 2.1.325). Read as the turn's end, that `result` would
/// complete the turn empty while Claude Code went on to answer nobody.
///
/// Claude Code started with `--replay-user-messages` echoes the prompt as it
/// takes it in, so until that echo nothing it writes is this turn's: messages,
/// stream events, tool results and task events are dropped, and a successful
/// `result` is swallowed whatever text it carries. Only the session id and a
/// failure get through. A command that replaces the CLI may not echo
/// (`expects_prompt_replay`); there the model's own output also shows the
/// prompt was taken in, and what Claude Code writes itself (model
/// `<synthetic>`) is still dropped.
#[derive(Debug, Default)]
pub struct ClaudeEventNormalizer {
    inner: AnthropicEventNormalizer,
    pending: Vec<PendingAgentMessage>,
    /// Main-chain tasks still owed a `task_notification`, by task id.
    background_tasks: HashMap<String, BackgroundTask>,
    /// Shell monitors have the same `local_bash` task type as detached servers.
    /// Match their launch to the Monitor tool call instead of holding every shell.
    monitor_calls: HashSet<String>,
    /// An explicit timeout distinguishes finite background work from servers.
    bounded_shell_calls: HashMap<String, Duration>,
    awaiting_follow_up: bool,
    /// Claude Code's own turn ended (its `result` was swallowed) and no
    /// follow-up message has started since.
    result_held: bool,
    /// Steers written to Claude Code that it has not echoed yet, by uuid.
    outstanding_steers: HashSet<String>,
    /// The uuid of this turn's prompt until Claude Code takes the prompt in.
    pending_turn_prompt: Option<String>,
    /// Claude Code echoes every prompt, so only the echo of the pending one
    /// shows it was taken in (see `expects_prompt_replay`).
    expect_prompt_replay: bool,
    /// Claude Code wrote a message of its own while the prompt was pending;
    /// the `result` after it closes that message, whatever text it repeats.
    pre_prompt_message: bool,
    /// The stream is inside a message Claude Code wrote itself: its
    /// `message_start` named the model `<synthetic>` and its `message_stop`
    /// has not arrived.
    in_synthetic_message: bool,
}

#[derive(Debug)]
struct BackgroundTask {
    item_raw_id: String,
    /// Listed by the latest `background_tasks_changed`. A task can drop out
    /// just before its notification lands, so it is still owed one.
    live: bool,
    deadline: Option<Instant>,
}

/// How long a held turn waits for Claude Code's follow-up once no background
/// task is live. Any output (including API retry notices) restarts it.
const FOLLOW_UP_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
struct PendingAgentMessage {
    item_id: String,
    chunks: Vec<String>,
    canonical: Option<String>,
}

impl PendingAgentMessage {
    fn text(&self) -> String {
        self.canonical
            .clone()
            .unwrap_or_else(|| self.chunks.concat())
    }
}

impl ClaudeEventNormalizer {
    /// Notes a steer sent with `uuid`, which this turn must see taken in.
    pub fn steer_sent(&mut self, uuid: String) {
        self.outstanding_steers.insert(uuid);
    }

    /// Notes this turn's prompt, sent with `uuid`. A `result` cannot answer
    /// it before Claude Code has taken it in. With `expect_replay`, only
    /// Claude Code's echo of `uuid` shows that it has.
    pub fn turn_prompt_sent(&mut self, uuid: String, expect_replay: bool) {
        self.pending_turn_prompt = Some(uuid);
        self.expect_prompt_replay = expect_replay;
        self.pre_prompt_message = false;
        self.result_held = false;
    }

    fn turn_prompt_taken_in(&mut self) {
        self.pending_turn_prompt = None;
        self.pre_prompt_message = false;
        // A `result` swallowed while the prompt waited was not this turn's.
        self.result_held = false;
    }

    pub fn awaiting_prompt_replay(&self) -> bool {
        self.pending_turn_prompt.is_some() && self.expect_prompt_replay
    }

    /// Whether `event` is part of a message Claude Code wrote itself, by the
    /// message's own model or, for a stream event, by the `message_start`
    /// that opened the message it belongs to.
    fn written_by_claude_code(&mut self, event: &AnthropicStreamEvent) -> bool {
        match event {
            AnthropicStreamEvent::Assistant { message, .. } => message.is_synthetic(),
            AnthropicStreamEvent::StreamEvent { event, .. } => match event {
                AnthropicRawStreamEvent::MessageStart { message } => {
                    self.in_synthetic_message = message.is_synthetic();
                    self.in_synthetic_message
                }
                AnthropicRawStreamEvent::MessageStop => {
                    std::mem::take(&mut self.in_synthetic_message)
                }
                _ => self.in_synthetic_message,
            },
            _ => false,
        }
    }

    pub fn normalize(&mut self, event: AnthropicStreamEvent) -> Vec<NormalizedEvent> {
        // An echoed user message carries no output of its own; it only tells
        // which prompt and steers Claude Code has taken in.
        if let AnthropicStreamEvent::User {
            is_replay: true,
            uuid,
            ..
        } = &event
        {
            if let Some(uuid) = uuid {
                self.outstanding_steers.remove(uuid);
                if self.pending_turn_prompt.as_ref() == Some(uuid) {
                    self.turn_prompt_taken_in();
                }
            }
            return Vec::new();
        }
        let synthetic = self.written_by_claude_code(&event);
        let mut pre_prompt_result = false;
        if self.pending_turn_prompt.is_some() && self.expect_prompt_replay {
            match &event {
                // The session id is the process's, whichever turn it runs.
                AnthropicStreamEvent::System { subtype, .. }
                    if subtype.as_deref() == Some("init") => {}
                // A failure ends the turn now: Claude Code that cannot start
                // or authenticate never echoes anything.
                AnthropicStreamEvent::Error { .. } => {}
                // A `result` that succeeded closes a turn Claude Code ran on
                // its own, whatever text it carries; one that failed is let
                // through (below) like any failure.
                AnthropicStreamEvent::Result { .. } => pre_prompt_result = true,
                // Everything else was written before Claude Code read the
                // prompt, so none of it answers the prompt: what the resumed
                // session says on its own, output cut off with its previous
                // process, the task events of work that died with it.
                _ => return Vec::new(),
            }
        } else if self.pending_turn_prompt.is_some() {
            match &event {
                // What Claude Code says on its own about the session it
                // resumed ("No response requested.") is no answer to a prompt
                // it has not read yet, and neither is any part of that
                // message's stream.
                AnthropicStreamEvent::StreamEvent { .. }
                | AnthropicStreamEvent::Assistant { .. }
                    if synthetic =>
                {
                    self.pre_prompt_message = true;
                    return Vec::new();
                }
                // The model only answers a prompt Claude Code has taken in,
                // which is the sign from a CLI that does not echo its input.
                AnthropicStreamEvent::StreamEvent { .. }
                | AnthropicStreamEvent::Assistant { .. } => self.turn_prompt_taken_in(),
                // A `result` with no text of its own answers nothing. One
                // that carries text and follows no message at all is left to
                // end the turn, as from a CLI that reports only its result.
                AnthropicStreamEvent::Result { result, .. } => {
                    pre_prompt_result = self.pre_prompt_message
                        || result.as_deref().is_none_or(|text| text.trim().is_empty());
                }
                _ => {}
            }
        }
        let token_usage = event.token_usage();
        let message_stop_reason = event.message_stop_reason().map(str::to_string);
        let mut out = Vec::new();
        self.track_background_tasks(&event, &mut out);
        let normalized = self.inner.normalize(event);
        // Claude Code ended a turn of its own before reading the prompt.
        let pre_prompt_result =
            pre_prompt_result && matches!(normalized, NormalizedEvent::Result { error: None });
        // What that turn used is not what this one did.
        if let Some(usage) = token_usage.filter(|_| !pre_prompt_result) {
            out.push(NormalizedEvent::TokenUsage { usage });
        }
        if let Some(stop_reason) = message_stop_reason {
            self.flush_pending(Some(stop_reason), &mut out);
        }

        match normalized {
            NormalizedEvent::AgentTextDelta { item_id, delta } => {
                self.flush_other_messages(&item_id, &mut out);
                self.pending_for(item_id).chunks.push(delta);
            }
            NormalizedEvent::AssistantMessage {
                partial: false,
                stop_reason,
                content,
            } => self.defer_assistant_message(stop_reason, content, &mut out),
            NormalizedEvent::ToolResults(results) => {
                self.flush_pending(Some("tool_use".to_string()), &mut out);
                out.push(NormalizedEvent::ToolResults(results));
            }
            NormalizedEvent::Result { error: None } if pre_prompt_result => {
                self.pre_prompt_message = false;
                self.result_held = true;
            }
            NormalizedEvent::Result { error: None } if self.holding_turn() => {
                self.flush_pending(Some("tool_use".to_string()), &mut out);
                self.result_held = true;
            }
            event @ NormalizedEvent::Result { .. } => {
                self.flush_pending(Some("end_turn".to_string()), &mut out);
                out.push(event);
            }
            event @ NormalizedEvent::Error { .. } => {
                self.flush_pending(None, &mut out);
                out.push(event);
            }
            NormalizedEvent::TokenUsage { .. } => {}
            NormalizedEvent::Ignored => {}
            event => out.push(event),
        }
        out
    }

    fn defer_assistant_message(
        &mut self,
        stop_reason: Option<String>,
        content: Vec<NormalizedContent>,
        out: &mut Vec<NormalizedEvent>,
    ) {
        let mut passthrough = Vec::new();
        let mut has_tool_use = false;
        for part in content {
            match part {
                NormalizedContent::AgentText { item_id, text } => {
                    self.flush_other_messages(&item_id, out);
                    self.pending_for(item_id).canonical = Some(text);
                }
                NormalizedContent::ToolUse {
                    raw_id,
                    tool,
                    arguments,
                } => {
                    if tool == "Monitor" {
                        self.monitor_calls.insert(raw_id.clone());
                    } else if tool == "Bash"
                        && arguments["run_in_background"] == true
                        && let Some(timeout) = arguments["timeout"].as_u64().filter(|ms| *ms > 0)
                    {
                        self.bounded_shell_calls
                            .insert(raw_id.clone(), Duration::from_millis(timeout));
                    }
                    has_tool_use = true;
                    passthrough.push(NormalizedContent::ToolUse {
                        raw_id,
                        tool,
                        arguments,
                    });
                }
                part @ NormalizedContent::ReasoningText { .. } => passthrough.push(part),
            }
        }
        if has_tool_use {
            self.flush_pending(Some("tool_use".to_string()), out);
        }
        if let Some(stop_reason) = stop_reason {
            self.flush_pending(Some(stop_reason), out);
        }
        if !passthrough.is_empty() {
            out.push(NormalizedEvent::AssistantMessage {
                partial: false,
                stop_reason: None,
                content: passthrough,
            });
        }
    }

    fn pending_for(&mut self, item_id: String) -> &mut PendingAgentMessage {
        if let Some(index) = self
            .pending
            .iter()
            .position(|message| message.item_id == item_id)
        {
            return &mut self.pending[index];
        }
        self.pending.push(PendingAgentMessage {
            item_id,
            chunks: Vec::new(),
            canonical: None,
        });
        self.pending.last_mut().expect("just pushed")
    }

    /// Text from an older message still pending when a newer message produces
    /// text means the older message ended mid-turn: commentary.
    fn flush_other_messages(&mut self, current_item_id: &str, out: &mut Vec<NormalizedEvent>) {
        if self
            .pending
            .iter()
            .all(|message| message.item_id == current_item_id)
        {
            return;
        }
        let (current, others) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|message| message.item_id == current_item_id);
        self.pending = current;
        flush_messages(others, Some("tool_use".to_string()), out);
    }

    fn flush_pending(&mut self, stop_reason: Option<String>, out: &mut Vec<NormalizedEvent>) {
        // A message that ends the model's turn while background tasks still
        // owe a follow-up is commentary, not the final answer; flushing it as
        // `end_turn` would also arm the terminal-stop fallback. An answer
        // given before a steer's follow-up still answers the user, so it
        // stays one (the runner arms no fallback while the turn is held).
        let stop_reason = match stop_reason {
            Some(reason) if reason != "tool_use" && self.holding_for_background_work() => {
                Some("tool_use".to_string())
            }
            reason => reason,
        };
        flush_messages(std::mem::take(&mut self.pending), stop_reason, out);
    }

    fn holding_turn(&self) -> bool {
        self.holding_for_background_work()
            || !self.outstanding_steers.is_empty()
            || self.holding_for_turn_prompt()
    }

    /// Claude Code ended a turn of its own and still owes the prompt an
    /// answer. The hold is bounded (`TurnHold::Idle`), so a CLI that never
    /// takes the prompt in fails the turn instead of hanging it.
    fn holding_for_turn_prompt(&self) -> bool {
        self.result_held && self.pending_turn_prompt.is_some()
    }

    fn holding_for_background_work(&self) -> bool {
        self.awaiting_follow_up || !self.background_tasks.is_empty()
    }

    pub fn turn_hold(&self) -> TurnHold {
        if !self.holding_turn() {
            TurnHold::Released
        } else if let Some(deadline) = self
            .background_tasks
            .values()
            .filter_map(|task| task.deadline)
            .min()
        {
            TurnHold::Until(deadline)
        } else if self.result_held && !self.background_tasks.values().any(|task| task.live) {
            TurnHold::Idle(FOLLOW_UP_IDLE_TIMEOUT)
        } else {
            TurnHold::Waiting
        }
    }

    fn track_background_tasks(
        &mut self,
        event: &AnthropicStreamEvent,
        out: &mut Vec<NormalizedEvent>,
    ) {
        let (subtype, task) = match event {
            AnthropicStreamEvent::System { subtype, task, .. } => (subtype.as_deref(), task),
            // The model is answering with every delivered notification in
            // context, whether mid-turn or in Claude Code's follow-up turn.
            AnthropicStreamEvent::StreamEvent {
                event: AnthropicRawStreamEvent::MessageStart { .. },
                ..
            } => {
                self.awaiting_follow_up = false;
                self.result_held = false;
                return;
            }
            _ => return,
        };
        match subtype {
            Some("task_started") => {
                let Some(task_id) = &task.task_id else { return };
                let monitor = task
                    .tool_use_id
                    .as_ref()
                    .is_some_and(|id| self.monitor_calls.remove(id));
                let deadline = task
                    .tool_use_id
                    .as_ref()
                    .and_then(|id| self.bounded_shell_calls.remove(id))
                    .filter(|_| {
                        task.is_backgrounded && task.task_type.as_deref() == Some("local_bash")
                    })
                    .and_then(|timeout| Instant::now().checked_add(timeout))
                    // Let Claude report the native timeout before expiring the execution.
                    .and_then(|deadline| deadline.checked_add(FOLLOW_UP_IDLE_TIMEOUT));
                if task.owned_by_subagent
                    || !(monitor
                        || deadline.is_some()
                        || task.is_backgrounded && task.is_main_chain_agent())
                {
                    return;
                }
                // Unique per run: a resumed agent reuses its task id.
                let item_raw_id = format!("background-task-{}", Uuid::new_v4().simple());
                self.background_tasks.insert(
                    task_id.clone(),
                    BackgroundTask {
                        item_raw_id: item_raw_id.clone(),
                        live: true,
                        deadline,
                    },
                );
                out.push(NormalizedEvent::AssistantMessage {
                    partial: false,
                    stop_reason: None,
                    content: vec![NormalizedContent::ToolUse {
                        raw_id: item_raw_id,
                        tool: if monitor {
                            "BackgroundMonitor"
                        } else if deadline.is_some() {
                            "BackgroundCommand"
                        } else {
                            BACKGROUND_AGENT_TOOL
                        }
                        .to_string(),
                        arguments: json!({
                            "description": task.description,
                            "subagent_type": task.subagent_type,
                        }),
                    }],
                });
            }
            Some("task_notification") => {
                // Notifications carry no task type; match the launch by id.
                let Some(task_id) = &task.task_id else { return };
                let Some(task_run) = self.background_tasks.remove(task_id) else {
                    return;
                };
                self.awaiting_follow_up = true;
                out.push(NormalizedEvent::ToolResults(vec![NormalizedToolResult {
                    tool_use_id: task_run.item_raw_id,
                    content: task.summary.clone().unwrap_or_default(),
                    is_error: task.status.as_deref() != Some("completed"),
                    exit_code: None,
                }]));
            }
            Some("background_tasks_changed") => {
                let Some(tasks) = &task.tasks else { return };
                for (task_id, task_run) in &mut self.background_tasks {
                    task_run.live = tasks
                        .iter()
                        .any(|task| task.task_id.as_ref() == Some(task_id));
                }
            }
            _ => {}
        }
    }
}

const BACKGROUND_AGENT_TOOL: &str = "BackgroundAgent";

fn flush_messages(
    pending: Vec<PendingAgentMessage>,
    stop_reason: Option<String>,
    out: &mut Vec<NormalizedEvent>,
) {
    for message in pending {
        let text = message.text();
        if text.is_empty() {
            continue;
        }
        out.push(NormalizedEvent::AgentMessageStarted {
            item_id: message.item_id.clone(),
            stop_reason: stop_reason.clone(),
        });
        out.extend(
            message
                .chunks
                .into_iter()
                .map(|delta| NormalizedEvent::AgentTextDelta {
                    item_id: message.item_id.clone(),
                    delta,
                }),
        );
        out.push(NormalizedEvent::AssistantMessage {
            partial: false,
            stop_reason: stop_reason.clone(),
            content: vec![NormalizedContent::AgentText {
                item_id: message.item_id,
                text,
            }],
        });
    }
}

#[derive(Debug, Default)]
pub struct ClaudeCodeHarness;

impl HarnessServer for ClaudeCodeHarness {
    type Event = AnthropicStreamEvent;
    type EventNormalizer = ClaudeEventNormalizer;

    fn kind(&self) -> HarnessKind {
        HarnessKind::ClaudeCode
    }

    fn cli_version(&self) -> &'static str {
        "claude-code"
    }

    /// Empty when no explicit override exists: the model is owned by the
    /// in-image harness config (harness/claude/settings.json), mirroring how
    /// codex reads harness/codex/config.toml. An empty model means
    /// `command_for_turn` omits `--model` so the CLI falls through to
    /// settings.json.
    fn default_model(&self) -> String {
        env::var("CLAUDE_MODEL").unwrap_or_default()
    }

    fn default_model_provider(&self) -> &'static str {
        "anthropic"
    }

    fn command_for_turn(&self, state: &ThreadState) -> ProcessCommand {
        if let Some(command) = command_from_override(BRIDGE_COMMAND_ENV) {
            return command;
        }

        let bin = env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
        let mut command = ProcessCommand::new(bin);
        command.args([
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--dangerously-skip-permissions",
            "--permission-mode",
            "bypassPermissions",
            // Echoes each user message as Claude Code takes it in, which is
            // how a turn tells whether a steer still owes an answer.
            "--replay-user-messages",
        ]);
        if state.supports_user_input {
            command.args(["--permission-prompt-tool", "stdio"]);
        }
        let mut guidance = String::from(
            "For short, bounded watches, use native Monitor and stop on the outcome or expiry; report actionable changes only. Use recurring schedules only for recurring requests.",
        );
        if state.supports_user_input {
            guidance.push(' ');
            guidance.push_str(QUESTION_GUIDANCE);
        }
        command.args(["--append-system-prompt", &guidance]);
        if state.supports_prompt_suggestions {
            command.arg("--prompt-suggestions");
        }
        if !state.model.is_empty() {
            command.args(["--model", &state.model]);
        }
        if PathBuf::from("AGENTS.md").is_file() {
            command.args(["--append-system-prompt-file", "AGENTS.md"]);
        }
        if let Some(session_id) = &state.harness_session_id {
            command.args(["--resume", session_id]);
        } else {
            command.args(["--session-id", &state.id]);
        }
        command
    }

    /// The prompt carries a `uuid`, as a steer does, so the turn can tell
    /// when Claude Code has taken it in.
    fn stdin_for_turn(
        &self,
        normalizer: &mut Self::EventNormalizer,
        input: &[UserInput],
    ) -> Result<Vec<u8>> {
        let uuid = Uuid::new_v4().to_string();
        let payload = json!({
            "type": "user",
            "uuid": uuid,
            "message": {
                "role": "user",
                "content": user_input_to_anthropic_content(input),
            },
        });
        normalizer.turn_prompt_sent(uuid, expects_prompt_replay());
        let mut bytes = serde_json::to_vec(&payload)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn steers_mid_turn(&self) -> bool {
        true
    }

    fn stdin_for_steer(
        &self,
        normalizer: &mut Self::EventNormalizer,
        input: &[UserInput],
    ) -> Result<Vec<u8>> {
        let uuid = Uuid::new_v4().to_string();
        let payload = json!({
            "type": "user",
            "uuid": uuid,
            "message": {
                "role": "user",
                "content": user_input_to_anthropic_content(input),
            },
        });
        normalizer.steer_sent(uuid);
        let mut bytes = serde_json::to_vec(&payload)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn reasoning_effort(&self, requested: &str) -> Option<String> {
        let effort = requested.trim().to_ascii_lowercase();
        if CLAUDE_EFFORT_LEVELS.contains(&effort.as_str()) {
            return Some(effort);
        }
        eprintln!("ignoring unsupported Claude Code effort level {requested:?}");
        None
    }

    /// The Claude process outlives each turn, so effort is applied in-band
    /// with an `apply_flag_settings` control request ahead of the turn's user
    /// message. A `null` effortLevel restores the configured default.
    fn stdin_for_reasoning_effort(&self, effort: Option<&str>) -> Result<Vec<u8>> {
        let payload = json!({
            "type": "control_request",
            "request_id": format!("effort-{}", Uuid::new_v4().simple()),
            "request": {
                "subtype": "apply_flag_settings",
                "settings": { "effortLevel": effort },
            },
        });
        let mut bytes = serde_json::to_vec(&payload)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// A subagent sidechain's event is not read past its envelope, so a body
    /// this server cannot read does not fail the parent turn.
    fn parse_stdout_line(&self, line: &str) -> Result<Self::Event> {
        AnthropicStreamEvent::parse_main_chain_json_line(line)
    }

    fn user_input_control(&self, line: &str) -> Result<Option<UserInputControl>> {
        // Inspect only the top-level envelope before deserializing `Value` so
        // ordinary stream events (including deeply nested subagent sidechains)
        // never pay full `Value` parsing or hit `serde_json` recursion limits
        // ahead of `parse_stdout_line`'s sidechain filter.
        let Some(envelope) = crate::anthropic::scan_top_level_envelope(line) else {
            return Ok(None);
        };
        if !matches!(
            envelope.kind.as_deref(),
            Some("control_request" | "control_cancel_request")
        ) {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(line)?;
        if value["type"] == "control_cancel_request" {
            return Ok(Some(UserInputControl::Cancel(value["request_id"].clone())));
        }
        if value["type"] != "control_request" {
            return Ok(None);
        }
        let id = value["request_id"].clone();
        let request = &value["request"];
        if request["subtype"] == "can_use_tool" && request["tool_name"] == "AskUserQuestion" {
            let input = request["input"].clone();
            let Some(questions) = input["questions"].as_array() else {
                return Ok(Some(rejected_question_control(id)?));
            };
            let mut normalized = Vec::with_capacity(questions.len());
            let mut texts = std::collections::HashSet::new();
            for (index, question) in questions.iter().enumerate() {
                if !texts.insert(question["question"].as_str().unwrap_or_default()) {
                    return Ok(Some(rejected_question_control(id)?));
                }
                let mut question = question.clone();
                question["id"] = json!(format!("question_{}", index + 1));
                question["allowOther"] = json!(true);
                let Ok(question) = serde_json::from_value::<Question>(question) else {
                    return Ok(Some(rejected_question_control(id)?));
                };
                normalized.push(question);
            }
            return Ok(Some(UserInputControl::Request(NativeUserInputRequest {
                native_id: id,
                item_id: request["tool_use_id"]
                    .as_str()
                    .unwrap_or("AskUserQuestion")
                    .to_owned(),
                questions: normalized,
                input,
                is_blocking: true,
            })));
        }
        // Enabling the native question channel must not silently approve any
        // other permission request. Ordinary bypassPermissions behavior stays
        // in the CLI; requests it still escalates remain denied here.
        let response = if request["subtype"] == "can_use_tool" {
            json!({"subtype":"success","request_id":id,"response":{"behavior":"deny","message":"Centaur only handles AskUserQuestion on this input channel"}})
        } else {
            json!({"subtype":"error","request_id":id,"error":"Unsupported host control request"})
        };
        Ok(Some(UserInputControl::Reply(json_line(
            &json!({"type":"control_response","response":response}),
        )?)))
    }

    fn stdin_for_user_input_response(
        &self,
        request: &NativeUserInputRequest,
        answers: &Answers,
    ) -> Result<Vec<u8>> {
        let mut input = request.input.clone();
        let native_answers = request
            .questions
            .iter()
            .map(|question| {
                (
                    question.question.clone(),
                    json!(answers[&question.id].answers.join(", ")),
                )
            })
            .collect::<serde_json::Map<String, Value>>();
        input["answers"] = Value::Object(native_answers);
        json_line(&json!({"type":"control_response","response":{
            "subtype":"success","request_id":request.native_id,
            "response":{"behavior":"allow","updatedInput":input}
        }}))
    }

    fn stdin_for_user_input_rejection(&self, request: &NativeUserInputRequest) -> Result<Vec<u8>> {
        let UserInputControl::Reply(bytes) = rejected_question_control(request.native_id.clone())?
        else {
            unreachable!()
        };
        Ok(bytes)
    }

    fn accepts_user_input_request(&self, normalizer: &Self::EventNormalizer) -> bool {
        !normalizer.awaiting_prompt_replay()
    }

    fn normalize_events(
        &self,
        normalizer: &mut Self::EventNormalizer,
        event: Self::Event,
    ) -> Result<Vec<NormalizedEvent>> {
        // Subagent sidechains (Task tool) interleave their own messages into
        // the stream, ending with their own `end_turn` while the parent turn
        // keeps running. Letting them through corrupts the pending-text state
        // (their message ids clobber the main chain's) and their stop reasons
        // would settle — and with the stop fallback, terminate — the parent
        // turn. The subagent's report reaches the turn through the main
        // chain's Task tool result.
        if event.is_sidechain() {
            return Ok(Vec::new());
        }
        Ok(normalizer.normalize(event))
    }

    /// Claude Code normally ends a turn with a native `result` line, but
    /// streams have been observed to stop at `message_delta.stop_reason`
    /// without one (leaving the execution hung as "thinking" forever). Wait a
    /// short window for the native result before completing on the stop, so
    /// the trailing `result` is consumed by this turn instead of instantly
    /// terminating the next one.
    fn terminal_assistant_stop_settle(&self) -> Option<Duration> {
        Some(Duration::from_secs(2))
    }

    fn turn_hold(&self, normalizer: &Self::EventNormalizer) -> TurnHold {
        normalizer.turn_hold()
    }

    fn prompt_suggestion_window(&self) -> Option<Duration> {
        Some(Duration::from_secs(2))
    }

    fn prompt_suggestion(&self, line: &str, session_id: Option<&str>) -> Option<String> {
        let value: Value = serde_json::from_str(line).ok()?;
        if value["type"] != "prompt_suggestion" || value["session_id"].as_str() != Some(session_id?)
        {
            return None;
        }
        let suggestion = value["suggestion"].as_str()?.trim();
        if suggestion.is_empty()
            || suggestion.chars().count() > 500
            || suggestion.chars().any(char::is_control)
        {
            return None;
        }
        Some(suggestion.to_owned())
    }
}

/// Replaces the Claude Code command (tests, a bridge to another runtime).
const BRIDGE_COMMAND_ENV: &str = "CENTAUR_CLAUDE_APP_BRIDGE_COMMAND";
/// States whether the command echoes each prompt it takes in: `1` or `true`
/// that it does, `0` or `false` that it does not.
const EXPECT_USER_REPLAY_ENV: &str = "CENTAUR_CLAUDE_EXPECT_USER_REPLAY";

/// Whether the Claude Code this server runs echoes each prompt it takes in,
/// so that nothing before the echo can be the prompt's answer.
fn expects_prompt_replay() -> bool {
    prompt_replay_expected(
        env::var(BRIDGE_COMMAND_ENV).ok().as_deref(),
        env::var(EXPECT_USER_REPLAY_ENV).ok().as_deref(),
    )
}

/// The native CLI echoes: `command_for_turn` starts it with
/// `--replay-user-messages`. A command that replaces it is not known to, so
/// the model's output is its sign there unless it states that it echoes.
/// Stating that the CLI does not echo restores that reading for the native
/// CLI too.
fn prompt_replay_expected(bridge_command: Option<&str>, expect_user_replay: Option<&str>) -> bool {
    match expect_user_replay.map(str::trim) {
        Some("1" | "true") => true,
        Some("0" | "false") => false,
        _ => bridge_command.is_none_or(|command| command.trim().is_empty()),
    }
}

fn rejected_question_control(native_id: Value) -> Result<UserInputControl> {
    Ok(UserInputControl::Reply(json_line(
        &json!({"type":"control_response","response":{
            "subtype":"success","request_id":native_id,
            "response":{"behavior":"deny","message":"This question request is unsupported or exceeds the chat input limits"}
        }}),
    )?))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use crate::protocol::UserInput;
    use crate::{
        HarnessServer, NormalizedContent, NormalizedEvent, TurnHold,
        anthropic::AnthropicStreamEvent,
    };

    use super::{
        ClaudeCodeHarness, ClaudeEventNormalizer, FOLLOW_UP_IDLE_TIMEOUT, prompt_replay_expected,
    };
    use crate::user_input::UserInputControl;

    #[test]
    fn native_prompt_suggestions_are_opt_in() {
        let mut state = ClaudeCodeHarness.thread_state(
            &crate::protocol::ThreadStartParams::default(),
            std::env::temp_dir(),
        );
        for enabled in [false, true] {
            state.supports_prompt_suggestions = enabled;
            let command = ClaudeCodeHarness.command_for_turn(&state);
            assert_eq!(
                command.get_args().any(|arg| arg == "--prompt-suggestions"),
                enabled,
            );
        }
    }

    #[test]
    fn native_prompt_suggestions_reject_malformed_or_foreign_output() {
        let harness = ClaudeCodeHarness;
        let valid = json!({
            "type": "prompt_suggestion",
            "session_id": "native-session",
            "suggestion": "  Run the tests  ",
        });
        assert_eq!(
            harness.prompt_suggestion(&valid.to_string(), Some("native-session")),
            Some("Run the tests".to_string()),
        );
        assert_eq!(harness.prompt_suggestion(&valid.to_string(), None), None);
        assert_eq!(
            harness.prompt_suggestion(&valid.to_string(), Some("different-session")),
            None,
        );
        for suggestion in [
            json!(""),
            json!(" \n "),
            json!("x".repeat(501)),
            json!("a\nb"),
            json!(7),
        ] {
            let mut invalid = valid.clone();
            invalid["suggestion"] = suggestion;
            assert_eq!(
                harness.prompt_suggestion(&invalid.to_string(), Some("native-session")),
                None,
            );
        }
        assert_eq!(
            harness.prompt_suggestion("not JSON", Some("native-session")),
            None
        );
        assert_eq!(
            harness.prompt_suggestion(
                &json!({"type":"result","session_id":"native-session","suggestion":"ignored"})
                    .to_string(),
                Some("native-session"),
            ),
            None,
        );
    }

    #[test]
    fn native_questions_return_all_answers_in_claudes_original_shape() {
        let original = json!({"type":"control_request","request_id":"native-1","request":{
            "subtype":"can_use_tool","tool_name":"AskUserQuestion","tool_use_id":"tool-1",
            "input":{"questions":[
                {"header":"Project","question":"Which project?","options":[{"label":"Alpha","description":"Fits"},{"label":"Beta","description":"Fits too"}],"multiSelect":false},
                {"header":"Scope","question":"Which features?","options":[{"label":"API","description":"Server"},{"label":"UI","description":"Client"}],"multiSelect":true}
            ],"metadata":{"source":"native"}}
        }});
        let Some(UserInputControl::Request(request)) = ClaudeCodeHarness
            .user_input_control(&original.to_string())
            .unwrap()
        else {
            panic!("missing question");
        };
        assert!(request.questions[1].multi_select);
        let answers = std::collections::BTreeMap::from([
            (
                "question_1".into(),
                crate::user_input::UserInputAnswer {
                    answers: vec!["Custom project".into()],
                },
            ),
            (
                "question_2".into(),
                crate::user_input::UserInputAnswer {
                    answers: vec!["API".into(), "UI".into()],
                },
            ),
        ]);
        let response: Value = serde_json::from_slice(
            &ClaudeCodeHarness
                .stdin_for_user_input_response(&request, &answers)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(response["response"]["request_id"], "native-1");
        let input = &response["response"]["response"]["updatedInput"];
        assert_eq!(
            input["questions"],
            original["request"]["input"]["questions"]
        );
        assert_eq!(input["metadata"]["source"], "native");
        assert_eq!(
            input["answers"],
            json!({"Which project?":"Custom project","Which features?":"API, UI"})
        );
    }

    #[test]
    fn native_question_channel_does_not_approve_other_tools() {
        let control = json!({"type":"control_request","request_id":"permission-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"echo should-not-run"}}});
        let Some(UserInputControl::Reply(bytes)) = ClaudeCodeHarness
            .user_input_control(&control.to_string())
            .unwrap()
        else {
            panic!("missing denial");
        };
        let response: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response["response"]["response"]["behavior"], "deny");
    }

    #[test]
    fn user_input_control_skips_deeply_nested_non_control_events() {
        let deep = format!("{}0{}", r#"{"a":"#.repeat(200), "}".repeat(200));
        let sidechain = format!(
            r#"{{"type":"assistant","parent_tool_use_id":"toolu_task","message":{{"id":"m1","content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{deep}}}]}}}}"#
        );
        assert!(
            ClaudeCodeHarness
                .user_input_control(&sidechain)
                .expect("non-control line must not fail control scan")
                .is_none()
        );
    }

    #[test]
    fn strict_prompt_replay_gates_user_input_requests_until_prompt_echo() {
        let mut normalizer = ClaudeEventNormalizer::default();
        normalizer.turn_prompt_sent("prompt-1".to_string(), true);
        assert!(!ClaudeCodeHarness.accepts_user_input_request(&normalizer));
        // Foreign replay does not open the gate.
        normalize(
            &mut normalizer,
            json!({"type":"user","isReplay":true,"uuid":"other","message":{"role":"user","content":[]}}),
        );
        assert!(!ClaudeCodeHarness.accepts_user_input_request(&normalizer));
        // Matching echo opens the gate.
        normalize(
            &mut normalizer,
            json!({"type":"user","isReplay":true,"uuid":"prompt-1","message":{"role":"user","content":[]}}),
        );
        assert!(ClaudeCodeHarness.accepts_user_input_request(&normalizer));

        // No-echo compatibility mode keeps questions open.
        let mut compat = ClaudeEventNormalizer::default();
        compat.turn_prompt_sent("prompt-2".to_string(), false);
        assert!(ClaudeCodeHarness.accepts_user_input_request(&compat));
    }

    fn normalize(normalizer: &mut ClaudeEventNormalizer, event: Value) -> Vec<NormalizedEvent> {
        normalizer.normalize(serde_json::from_value(event).unwrap())
    }

    fn agent_texts(events: &[NormalizedEvent]) -> Vec<(Option<String>, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                NormalizedEvent::AssistantMessage {
                    partial: false,
                    stop_reason,
                    content,
                } => {
                    let text = content
                        .iter()
                        .filter_map(|part| match part {
                            NormalizedContent::AgentText { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    (!text.is_empty()).then(|| (stop_reason.clone(), text))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn defers_streamed_text_until_message_delta_settles_stop_reason() {
        let mut normalizer = ClaudeEventNormalizer::default();

        let events = normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "message_start", "message": {"id": "msg_1", "stop_reason": null, "content": []}}}),
        );
        assert!(events.is_empty());

        let events = normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}}),
        );
        assert!(events.is_empty());

        // Text deltas are buffered, not forwarded.
        let events = normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Let me check."}}}),
        );
        assert!(events.is_empty());

        // The per-block assistant event records canonical text but stays deferred.
        let events = normalize(
            &mut normalizer,
            json!({"type": "assistant", "message": {"id": "msg_1", "stop_reason": null, "content": [{"type": "text", "text": "Let me check."}]}}),
        );
        assert!(events.is_empty());

        // The tool_use block settles the message as commentary.
        let events = normalize(
            &mut normalizer,
            json!({"type": "assistant", "message": {"id": "msg_1", "stop_reason": null, "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "echo hello"}}]}}),
        );
        assert_eq!(
            agent_texts(&events),
            vec![(Some("tool_use".to_string()), "Let me check.".to_string())]
        );
        assert!(matches!(
            events.last(),
            Some(NormalizedEvent::AssistantMessage { content, .. })
                if matches!(content.as_slice(), [NormalizedContent::ToolUse { .. }])
        ));

        // message_delta arrives after the blocks; nothing left to flush.
        let events = normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "message_delta", "delta": {"stop_reason": "tool_use"}}}),
        );
        assert!(events.is_empty());
    }

    #[test]
    fn final_message_flushes_as_end_turn_on_message_delta() {
        let mut normalizer = ClaudeEventNormalizer::default();
        normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "message_start", "message": {"id": "msg_2", "stop_reason": null, "content": []}}}),
        );
        normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}}),
        );
        normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "DONE"}}}),
        );
        normalize(
            &mut normalizer,
            json!({"type": "assistant", "message": {"id": "msg_2", "stop_reason": null, "content": [{"type": "text", "text": "DONE"}]}}),
        );

        let events = normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "message_delta", "delta": {"stop_reason": "end_turn"}}}),
        );
        assert_eq!(
            agent_texts(&events),
            vec![(Some("end_turn".to_string()), "DONE".to_string())]
        );
    }

    #[test]
    fn pending_text_flushes_as_final_answer_when_result_arrives_first() {
        let mut normalizer = ClaudeEventNormalizer::default();
        normalize(
            &mut normalizer,
            json!({"type": "assistant", "message": {"id": "msg_1", "stop_reason": null, "content": [{"type": "text", "text": "final answer"}]}}),
        );

        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "final answer"}),
        );
        assert_eq!(
            agent_texts(&events),
            vec![(Some("end_turn".to_string()), "final answer".to_string())]
        );
        assert!(matches!(
            events.last(),
            Some(NormalizedEvent::Result { error: None })
        ));
    }

    #[test]
    fn tool_result_flushes_pending_text_as_commentary() {
        let mut normalizer = ClaudeEventNormalizer::default();
        normalize(
            &mut normalizer,
            json!({"type": "assistant", "message": {"id": "msg_1", "stop_reason": null, "content": [{"type": "text", "text": "Let me check."}]}}),
        );

        let events = normalize(
            &mut normalizer,
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok", "is_error": false}]}}),
        );
        assert_eq!(
            agent_texts(&events),
            vec![(Some("tool_use".to_string()), "Let me check.".to_string())]
        );
        assert!(matches!(
            events.last(),
            Some(NormalizedEvent::ToolResults(results)) if results.len() == 1
        ));
    }

    #[test]
    fn newer_message_text_flushes_older_pending_message_as_commentary() {
        let mut normalizer = ClaudeEventNormalizer::default();
        normalize(
            &mut normalizer,
            json!({"type": "assistant", "message": {"id": "msg_1", "stop_reason": null, "content": [{"type": "text", "text": "first"}]}}),
        );

        let events = normalize(
            &mut normalizer,
            json!({"type": "assistant", "message": {"id": "msg_2", "stop_reason": null, "content": [{"type": "text", "text": "second"}]}}),
        );
        assert_eq!(
            agent_texts(&events),
            vec![(Some("tool_use".to_string()), "first".to_string())]
        );

        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "second"}),
        );
        assert_eq!(
            agent_texts(&events),
            vec![(Some("end_turn".to_string()), "second".to_string())]
        );
    }

    #[test]
    fn flush_replays_original_delta_chunks_behind_phased_item_start() {
        let mut normalizer = ClaudeEventNormalizer::default();
        normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "message_start", "message": {"id": "msg_1", "stop_reason": null, "content": []}}}),
        );
        normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}}),
        );
        for chunk in ["hel", "lo ", "world"] {
            normalize(
                &mut normalizer,
                json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": chunk}}}),
            );
        }

        let events = normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "message_delta", "delta": {"stop_reason": "end_turn"}}}),
        );
        assert!(matches!(
            &events[0],
            NormalizedEvent::AgentMessageStarted { item_id, stop_reason: Some(reason) }
                if item_id == "msg_1" && reason == "end_turn"
        ));
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                NormalizedEvent::AgentTextDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["hel", "lo ", "world"]);
        assert_eq!(
            agent_texts(&events),
            vec![(Some("end_turn".to_string()), "hello world".to_string())]
        );
    }

    #[test]
    fn explicit_assistant_stop_reason_flushes_immediately() {
        let mut normalizer = ClaudeEventNormalizer::default();
        let events = normalize(
            &mut normalizer,
            json!({"type": "assistant", "message": {"id": "msg_1", "stop_reason": "end_turn", "content": [{"type": "text", "text": "hello"}]}}),
        );
        assert_eq!(
            agent_texts(&events),
            vec![(Some("end_turn".to_string()), "hello".to_string())]
        );
    }

    /// Replays the shape Claude Code 2.1 emits for a background subagent: the
    /// launching turn ends with its own `result`, then the CLI runs a
    /// follow-up turn once the agent's `task_notification` lands.
    #[test]
    fn background_agent_holds_turn_open_until_follow_up_result() {
        background_task_follow_up("Agent", "local_agent");
    }

    #[test]
    fn monitor_holds_turn_open_until_follow_up_result() {
        // A real shell Monitor reports local_bash, just like a detached server.
        // WebSocket and MCP monitors share the same tool-call correlation.
        for task_type in ["local_bash", "monitor_ws", "monitor_mcp"] {
            background_task_follow_up("Monitor", task_type);
        }
    }

    fn background_task_follow_up(tool: &str, task_type: &str) {
        let harness = ClaudeCodeHarness;
        let mut normalizer = ClaudeEventNormalizer::default();
        let mut feed = |event: Value| {
            harness
                .normalize_events(&mut normalizer, serde_json::from_value(event).unwrap())
                .unwrap()
        };
        let msg = |id: &str, text: &str| {
            [
                json!({"type": "stream_event", "event": {"type": "message_start", "message": {"id": id, "content": []}}}),
                json!({"type": "assistant", "message": {"id": id, "content": [{"type": "text", "text": text}]}}),
                json!({"type": "stream_event", "event": {"type": "message_delta", "delta": {"stop_reason": "end_turn"}}}),
            ]
        };

        feed(
            json!({"type": "assistant", "message": {"id": "msg_1", "content": [{"type": "tool_use", "id": "toolu_agent", "name": tool, "input": {}}]}}),
        );
        feed(
            json!({"type": "system", "subtype": "background_tasks_changed", "tasks": [{"task_id": "a1", "task_type": task_type}]}),
        );
        let started = feed(
            json!({"type": "system", "subtype": "task_started", "task_id": "a1", "tool_use_id": "toolu_agent", "description": "Research prices", "subagent_type": "general-purpose", "is_backgrounded": true, "task_type": task_type}),
        );
        let item_raw_id = background_task_raw_id(&started);
        assert!(matches!(
            &started[0],
            NormalizedEvent::AssistantMessage { content, .. }
                if matches!(&content[0], NormalizedContent::ToolUse { tool: name, .. }
                    if name == &format!("Background{tool}"))
        ));
        feed(
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_agent", "content": "Async agent launched successfully."}]}}),
        );

        // The launching turn's closing text is commentary and its result is
        // swallowed, so neither settles the turn.
        let mut events = Vec::new();
        for event in msg("msg_2", "Started research.") {
            events.extend(feed(event));
        }
        events.extend(feed(
            json!({"type": "result", "subtype": "success", "result": "Started research."}),
        ));
        assert_eq!(
            agent_texts(&events),
            vec![(
                Some("tool_use".to_string()),
                "Started research.".to_string()
            )]
        );
        assert!(
            !events
                .iter()
                .any(|event| event.is_terminal() || event.is_terminal_assistant_stop())
        );

        // Sidechain chatter stays invisible.
        assert!(feed(json!({"type": "assistant", "parent_tool_use_id": "toolu_agent", "message": {"id": "msg_side", "stop_reason": "end_turn", "content": [{"type": "text", "text": "PONG"}]}})).is_empty());

        // The live set drops the agent just before its notification lands.
        feed(json!({"type": "system", "subtype": "background_tasks_changed", "tasks": []}));
        let notified = feed(
            json!({"type": "system", "subtype": "task_notification", "task_id": "a1", "tool_use_id": "toolu_agent", "status": "completed", "summary": "Prices are up 4%."}),
        );
        assert!(matches!(
            notified.as_slice(),
            [NormalizedEvent::ToolResults(results)]
                if results[0].tool_use_id == item_raw_id
                    && results[0].content == "Prices are up 4%."
                    && !results[0].is_error
        ));

        // Claude Code's follow-up turn carries the real answer and terminates.
        feed(json!({"type": "system", "subtype": "init", "session_id": "s1"}));
        let mut events = Vec::new();
        for event in msg("msg_3", "Prices are up 4%.") {
            events.extend(feed(event));
        }
        assert_eq!(
            agent_texts(&events),
            vec![(
                Some("end_turn".to_string()),
                "Prices are up 4%.".to_string()
            )]
        );
        let done =
            feed(json!({"type": "result", "subtype": "success", "result": "Prices are up 4%."}));
        assert!(done.iter().any(NormalizedEvent::is_terminal));
    }

    fn background_task_raw_id(events: &[NormalizedEvent]) -> String {
        match events {
            [NormalizedEvent::AssistantMessage { content, .. }] => match content.as_slice() {
                [NormalizedContent::ToolUse { raw_id, tool, .. }]
                    if matches!(tool.as_str(), "BackgroundAgent" | "BackgroundMonitor") =>
                {
                    raw_id.clone()
                }
                other => panic!("expected a BackgroundAgent tool use, got {other:?}"),
            },
            other => panic!("expected a BackgroundAgent tool use, got {other:?}"),
        }
    }

    #[test]
    fn held_turn_waits_on_live_agents_and_times_out_only_when_idle() {
        let mut normalizer = ClaudeEventNormalizer::default();
        normalize(
            &mut normalizer,
            json!({"type": "system", "subtype": "task_started", "task_id": "a1", "is_backgrounded": true, "task_type": "local_agent"}),
        );
        normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "started"}),
        );
        assert_eq!(normalizer.turn_hold(), TurnHold::Waiting);

        // Dropped from the live set without a notification: the follow-up may
        // never come, so the hold becomes bounded.
        normalize(
            &mut normalizer,
            json!({"type": "system", "subtype": "background_tasks_changed", "tasks": []}),
        );
        assert!(matches!(normalizer.turn_hold(), TurnHold::Idle(_)));

        normalize(
            &mut normalizer,
            json!({"type": "system", "subtype": "task_notification", "task_id": "a1", "status": "completed"}),
        );
        assert!(matches!(normalizer.turn_hold(), TurnHold::Idle(_)));

        normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "message_start", "message": {"id": "msg_2", "content": []}}}),
        );
        assert_eq!(normalizer.turn_hold(), TurnHold::Released);
    }

    #[test]
    fn resumed_agent_holds_again_with_a_new_item_and_duplicate_notifications_are_ignored() {
        let mut normalizer = ClaudeEventNormalizer::default();
        let started = json!({"type": "system", "subtype": "task_started", "task_id": "a1", "is_backgrounded": true, "task_type": "local_agent"});
        let notified = json!({"type": "system", "subtype": "task_notification", "task_id": "a1", "status": "completed"});
        let message_start = json!({"type": "stream_event", "event": {"type": "message_start", "message": {"id": "msg_2", "content": []}}});

        let first = background_task_raw_id(&normalize(&mut normalizer, started.clone()));
        normalize(&mut normalizer, notified.clone());
        normalize(&mut normalizer, message_start.clone());
        assert!(normalize(&mut normalizer, notified.clone()).is_empty());
        assert_eq!(normalizer.turn_hold(), TurnHold::Released);

        let resumed = background_task_raw_id(&normalize(&mut normalizer, started));
        assert_ne!(first, resumed);
        assert_eq!(normalizer.turn_hold(), TurnHold::Waiting);
        assert!(matches!(
            normalize(&mut normalizer, notified).as_slice(),
            [NormalizedEvent::ToolResults(results)] if results[0].tool_use_id == resumed
        ));
    }

    #[test]
    fn subagent_owned_agents_do_not_hold_the_turn() {
        let mut normalizer = ClaudeEventNormalizer::default();
        assert!(
            normalize(
                &mut normalizer,
                json!({"type": "system", "subtype": "task_started", "task_id": "a2", "is_backgrounded": true, "task_type": "local_agent", "owned_by_subagent": true}),
            )
            .is_empty()
        );
        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "done"}),
        );
        assert!(events.iter().any(NormalizedEvent::is_terminal));
    }

    #[test]
    fn unexpected_system_field_types_do_not_fail_the_line() {
        let event = ClaudeCodeHarness
            .parse_stdout_line(r#"{"type":"system","subtype":"status","is_backgrounded":null,"status":{"state":"busy"},"tasks":"none"}"#)
            .unwrap();
        assert!(matches!(event, AnthropicStreamEvent::System { .. }));
    }

    #[test]
    fn background_agent_finishing_before_final_message_still_waits_for_follow_up() {
        let mut normalizer = ClaudeEventNormalizer::default();
        normalize(
            &mut normalizer,
            json!({"type": "system", "subtype": "task_started", "task_id": "a1", "is_backgrounded": true, "task_type": "local_agent"}),
        );
        normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "message_start", "message": {"id": "msg_1", "content": []}}}),
        );
        // The notification lands while the final message is generating, so
        // the model has not seen it yet: the CLI will run a follow-up turn.
        normalize(
            &mut normalizer,
            json!({"type": "system", "subtype": "task_notification", "task_id": "a1", "status": "completed"}),
        );
        normalize(
            &mut normalizer,
            json!({"type": "assistant", "message": {"id": "msg_1", "content": [{"type": "text", "text": "waiting"}]}}),
        );
        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "waiting"}),
        );
        assert!(!events.iter().any(NormalizedEvent::is_terminal));

        normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "message_start", "message": {"id": "msg_2", "content": []}}}),
        );
        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "done"}),
        );
        assert!(events.iter().any(NormalizedEvent::is_terminal));
    }

    #[test]
    fn monitor_call_without_its_own_task_does_not_hold_the_turn() {
        for task in [
            json!({"tool_use_id": "unrelated", "task_type": "local_bash"}),
            json!({"tool_use_id": "toolu_monitor", "task_type": "local_bash", "owned_by_subagent": true}),
        ] {
            let mut normalizer = ClaudeEventNormalizer::default();
            normalize(
                &mut normalizer,
                json!({"type": "assistant", "message": {"id": "msg_1", "content": [{"type": "tool_use", "id": "toolu_monitor", "name": "Monitor", "input": {}}]}}),
            );
            // A rejected Monitor call alone cannot leave the turn waiting.
            assert_eq!(normalizer.turn_hold(), TurnHold::Released);
            let mut started = json!({"type": "system", "subtype": "task_started", "task_id": "b1", "is_backgrounded": true});
            started
                .as_object_mut()
                .unwrap()
                .extend(task.as_object().unwrap().clone());
            assert!(normalize(&mut normalizer, started).is_empty());
            let done = normalize(
                &mut normalizer,
                json!({"type": "result", "subtype": "success", "result": "done"}),
            );
            assert!(done.iter().any(NormalizedEvent::is_terminal));
        }
    }

    #[test]
    fn background_shell_tasks_do_not_hold_the_turn() {
        let mut normalizer = ClaudeEventNormalizer::default();
        normalize(
            &mut normalizer,
            json!({"type": "system", "subtype": "task_started", "task_id": "b1", "is_backgrounded": true, "task_type": "local_bash"}),
        );
        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "server started"}),
        );
        assert!(events.iter().any(NormalizedEvent::is_terminal));
    }

    #[test]
    fn bounded_shell_keeps_its_deadline_until_its_notification() {
        let mut normalizer = ClaudeEventNormalizer::default();
        normalize(
            &mut normalizer,
            json!({"type":"assistant","message":{"id":"launch","content":[{"type":"tool_use","id":"shell","name":"Bash","input":{"run_in_background":true,"timeout":1000}}]}}),
        );
        assert_eq!(normalizer.turn_hold(), TurnHold::Released);
        let started = json!({"type":"system","subtype":"task_started","task_id":"b1","tool_use_id":"shell","is_backgrounded":true,"task_type":"local_bash"});
        normalize(&mut normalizer, started.clone());
        let TurnHold::Until(deadline) = normalizer.turn_hold() else {
            panic!("bounded shell should hold its execution");
        };
        assert!(deadline > Instant::now());
        let events = normalize(
            &mut normalizer,
            json!({"type":"result","subtype":"success"}),
        );
        assert!(!events.iter().any(NormalizedEvent::is_terminal));
        // Neither repeated lifecycle events nor chatter extends a finite task.
        for event in [
            started,
            json!({"type":"system","subtype":"status"}),
            json!({"type":"system","subtype":"background_tasks_changed","tasks":[]}),
        ] {
            normalize(&mut normalizer, event);
            assert_eq!(normalizer.turn_hold(), TurnHold::Until(deadline));
        }
        // A noisy expired task stays expired; the server checks Until before
        // reading another line, without waiting for an idle receive timeout.
        let expired = Instant::now() - Duration::from_secs(1);
        normalizer.background_tasks.get_mut("b1").unwrap().deadline = Some(expired);
        normalize(&mut normalizer, json!({"type":"system","subtype":"status"}));
        assert!(
            matches!(normalizer.turn_hold(), TurnHold::Until(deadline) if deadline < Instant::now())
        );
        let events = normalize(
            &mut normalizer,
            json!({"type":"system","subtype":"task_notification","task_id":"b1","status":"failed","summary":"command timed out"}),
        );
        assert!(matches!(&events[0], NormalizedEvent::ToolResults(results) if results[0].is_error));
        assert_eq!(
            normalizer.turn_hold(),
            TurnHold::Idle(FOLLOW_UP_IDLE_TIMEOUT)
        );
        normalize(
            &mut normalizer,
            json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"answer","content":[]}}}),
        );
        let events = normalize(
            &mut normalizer,
            json!({"type":"result","subtype":"success"}),
        );
        assert!(events.iter().any(NormalizedEvent::is_terminal));
    }

    #[test]
    fn only_correlated_explicitly_bounded_main_chain_shells_hold_the_turn() {
        for (input, task) in [
            (json!({"run_in_background":true}), json!({})),
            (json!({"run_in_background":true,"timeout":0}), json!({})),
            (json!({"run_in_background":true,"timeout":-1}), json!({})),
            (
                json!({"run_in_background":true,"timeout":"1000"}),
                json!({}),
            ),
            (json!({"run_in_background":false,"timeout":1000}), json!({})),
            (
                json!({"run_in_background":true,"timeout":1000}),
                json!({"tool_use_id":"unrelated"}),
            ),
            (
                json!({"run_in_background":true,"timeout":1000}),
                json!({"owned_by_subagent":true}),
            ),
            (
                json!({"run_in_background":true,"timeout":1000}),
                json!({"is_backgrounded":false}),
            ),
        ] {
            let mut normalizer = ClaudeEventNormalizer::default();
            normalize(
                &mut normalizer,
                json!({"type":"assistant","message":{"id":"launch","content":[{"type":"tool_use","id":"shell","name":"Bash","input":input}]}}),
            );
            let mut started = json!({"type":"system","subtype":"task_started","task_id":"b1","tool_use_id":"shell","is_backgrounded":true,"task_type":"local_bash"});
            started
                .as_object_mut()
                .unwrap()
                .extend(task.as_object().unwrap().clone());
            assert!(normalize(&mut normalizer, started).is_empty());
            assert_eq!(normalizer.turn_hold(), TurnHold::Released);
        }
    }

    #[test]
    fn failed_result_ends_turn_even_with_background_agents_running() {
        let mut normalizer = ClaudeEventNormalizer::default();
        normalize(
            &mut normalizer,
            json!({"type": "system", "subtype": "task_started", "task_id": "a1", "is_backgrounded": true, "task_type": "local_agent"}),
        );
        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "error_during_execution", "is_error": true}),
        );
        assert!(events.iter().any(NormalizedEvent::is_terminal));
        // The agent still owes output, so the server stops it with the process.
        assert_ne!(normalizer.turn_hold(), TurnHold::Released);
    }

    #[test]
    fn steer_stdin_uses_claude_streaming_user_message_shape() {
        let mut normalizer = ClaudeEventNormalizer::default();
        let bytes = ClaudeCodeHarness
            .stdin_for_steer(
                &mut normalizer,
                &[UserInput::Text {
                    text: "new guidance".to_string(),
                    text_elements: Vec::new(),
                }],
            )
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(value["type"], "user");
        assert!(value.get("steer").is_none());
        assert_eq!(value["message"]["role"], "user");
        assert_eq!(value["message"]["content"][0]["text"], "new guidance");
        let uuid = value["uuid"].as_str().expect("steer uuid");
        assert!(normalizer.outstanding_steers.contains(uuid));
    }

    fn steer(normalizer: &mut ClaudeEventNormalizer) -> String {
        let bytes = ClaudeCodeHarness
            .stdin_for_steer(
                normalizer,
                &[UserInput::Text {
                    text: "also this".to_string(),
                    text_elements: Vec::new(),
                }],
            )
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        value["uuid"].as_str().unwrap().to_string()
    }

    fn replay(uuid: &str) -> Value {
        json!({
            "type": "user",
            "isReplay": true,
            "uuid": uuid,
            "message": {"role": "user", "content": [{"type": "text", "text": "also this"}]},
        })
    }

    #[test]
    fn steer_folded_into_a_tool_result_completes_on_the_result() {
        let mut normalizer = ClaudeEventNormalizer::default();
        let uuid = steer(&mut normalizer);
        // Claude Code folds the steer into the next tool result and echoes it.
        assert!(normalize(&mut normalizer, replay(&uuid)).is_empty());
        assert_eq!(normalizer.turn_hold(), TurnHold::Released);
        normalize(
            &mut normalizer,
            json!({"type": "assistant", "message": {"id": "msg_2", "stop_reason": "end_turn", "content": [{"type": "text", "text": "did both"}]}}),
        );
        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "did both"}),
        );
        assert!(events.iter().any(NormalizedEvent::is_terminal));
    }

    #[test]
    fn steer_answered_in_a_follow_up_holds_the_turn_until_its_result() {
        let mut normalizer = ClaudeEventNormalizer::default();
        normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "message_start", "message": {"id": "msg_1", "content": []}}}),
        );
        // The steer arrives while the final message generates: no tool result
        // is left to carry it, so Claude Code answers it after this `result`.
        let uuid = steer(&mut normalizer);
        let events = normalize(
            &mut normalizer,
            json!({"type": "assistant", "message": {"id": "msg_1", "stop_reason": "end_turn", "content": [{"type": "text", "text": "first part"}]}}),
        );
        // The answer given before the follow-up still answers the user.
        assert_eq!(
            agent_texts(&events),
            vec![(Some("end_turn".to_string()), "first part".to_string())]
        );
        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "first part"}),
        );
        assert!(!events.iter().any(NormalizedEvent::is_terminal));
        assert!(matches!(normalizer.turn_hold(), TurnHold::Idle(_)));

        assert!(normalize(&mut normalizer, replay(&uuid)).is_empty());
        normalize(
            &mut normalizer,
            json!({"type": "stream_event", "event": {"type": "message_start", "message": {"id": "msg_2", "content": []}}}),
        );
        assert_eq!(normalizer.turn_hold(), TurnHold::Released);
        normalize(
            &mut normalizer,
            json!({"type": "assistant", "message": {"id": "msg_2", "stop_reason": "end_turn", "content": [{"type": "text", "text": "steered"}]}}),
        );
        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "steered"}),
        );
        assert!(events.iter().any(NormalizedEvent::is_terminal));
    }

    #[test]
    fn replays_of_ordinary_prompts_produce_no_events() {
        let mut normalizer = ClaudeEventNormalizer::default();
        let events = normalize(&mut normalizer, replay("not-a-steer"));
        assert!(events.is_empty());
        assert_eq!(normalizer.turn_hold(), TurnHold::Released);
    }

    /// A prompt sent to a CLI that is not known to echo it: the model's
    /// output also shows the prompt was taken in.
    fn prompt(normalizer: &mut ClaudeEventNormalizer) -> String {
        prompt_sent(normalizer, false)
    }

    /// A prompt sent to Claude Code started with `--replay-user-messages`:
    /// nothing but its echo shows the prompt was taken in.
    fn echoed_prompt(normalizer: &mut ClaudeEventNormalizer) -> String {
        prompt_sent(normalizer, true)
    }

    fn prompt_sent(normalizer: &mut ClaudeEventNormalizer, expect_replay: bool) -> String {
        let uuid = uuid::Uuid::new_v4().to_string();
        normalizer.turn_prompt_sent(uuid.clone(), expect_replay);
        uuid
    }

    fn message_start(id: &str) -> Value {
        json!({"type": "stream_event", "event": {"type": "message_start", "message": {"id": id, "model": "claude-fable-5", "content": []}}})
    }

    fn stream(event: Value) -> Value {
        json!({"type": "stream_event", "event": event})
    }

    /// The whole stream of a message Claude Code wrote itself.
    fn synthetic_stream(text: &str) -> Vec<Value> {
        vec![
            stream(
                json!({"type": "message_start", "message": {"id": "synthetic-1", "model": "<synthetic>", "content": []}}),
            ),
            stream(
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            ),
            stream(
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
            ),
            synthetic(text),
            stream(json!({"type": "content_block_stop", "index": 0})),
            stream(json!({"type": "message_delta", "delta": {"stop_reason": "stop_sequence"}})),
            stream(json!({"type": "message_stop"})),
        ]
    }

    /// The model's answer as Claude Code streams it.
    fn answer_stream(id: &str, text: &str) -> Vec<Value> {
        vec![
            message_start(id),
            stream(
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            ),
            stream(
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
            ),
            json!({"type": "assistant", "message": {"id": id, "model": "claude-fable-5", "stop_reason": null, "content": [{"type": "text", "text": text}]}}),
            stream(json!({"type": "content_block_stop", "index": 0})),
            stream(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}})),
            stream(json!({"type": "message_stop"})),
        ]
    }

    fn normalize_all(
        normalizer: &mut ClaudeEventNormalizer,
        events: Vec<Value>,
    ) -> Vec<NormalizedEvent> {
        events
            .into_iter()
            .flat_map(|event| normalize(normalizer, event))
            .collect()
    }

    fn answer(id: &str, text: &str) -> Value {
        json!({"type": "assistant", "message": {"id": id, "model": "claude-fable-5", "stop_reason": "end_turn", "content": [{"type": "text", "text": text}]}})
    }

    fn synthetic(text: &str) -> Value {
        json!({"type": "assistant", "message": {"id": "synthetic-1", "model": "<synthetic>", "stop_reason": "stop_sequence", "content": [{"type": "text", "text": text}]}})
    }

    #[test]
    fn turn_stdin_carries_a_uuid_the_turn_waits_to_see_taken_in() {
        let mut normalizer = ClaudeEventNormalizer::default();
        let bytes = ClaudeCodeHarness
            .stdin_for_turn(
                &mut normalizer,
                &[UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
            )
            .unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        let value: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(value["type"], "user");
        assert_eq!(value["message"]["role"], "user");
        assert_eq!(value["message"]["content"][0]["text"], "hello");
        let uuid = value["uuid"].as_str().expect("prompt uuid").to_string();
        assert_eq!(normalizer.pending_turn_prompt.as_deref(), Some(&*uuid));
        // An unanswered prompt alone holds nothing: only a `result` that
        // comes before it is taken in does.
        assert_eq!(normalizer.turn_hold(), TurnHold::Released);
        let bytes = ClaudeCodeHarness
            .stdin_for_turn(&mut normalizer, &[])
            .unwrap();
        let next: Value = serde_json::from_slice(&bytes).unwrap();
        assert_ne!(next["uuid"], uuid, "each prompt has its own uuid");
        assert_eq!(
            normalizer.pending_turn_prompt.as_deref(),
            next["uuid"].as_str()
        );
    }

    #[test]
    fn only_a_command_that_replaces_the_cli_is_not_expected_to_echo_the_prompt() {
        // The native CLI is started with `--replay-user-messages`.
        assert!(prompt_replay_expected(None, None));
        assert!(prompt_replay_expected(Some("  "), None));
        assert!(prompt_replay_expected(None, Some("unrecognised")));
        // A command that replaces it is not known to echo...
        assert!(!prompt_replay_expected(Some("exec fake-claude"), None));
        assert!(!prompt_replay_expected(Some("exec fake-claude"), Some("")));
        // ...unless it says it does.
        for stated in ["1", "true", " 1 "] {
            assert!(prompt_replay_expected(
                Some("exec fake-claude"),
                Some(stated)
            ));
        }
        // Stating that the CLI does not echo applies to the native CLI too.
        for stated in ["0", "false"] {
            assert!(!prompt_replay_expected(None, Some(stated)));
            assert!(!prompt_replay_expected(
                Some("exec fake-claude"),
                Some(stated)
            ));
        }
    }

    /// The stream of a production turn (Claude Code 2.1.325): the previous
    /// process died with background agents running, and the resumed session
    /// closed that out with a `result` of its own 55ms before it dequeued the
    /// prompt. The turn completed empty while Claude Code ran on unheard.
    #[test]
    fn result_of_a_resumed_session_before_it_takes_the_prompt_in_does_not_end_the_turn() {
        for expect_replay in [false, true] {
            resumed_session_result_does_not_end_the_turn(expect_replay);
        }
    }

    fn resumed_session_result_does_not_end_the_turn(expect_replay: bool) {
        let mut normalizer = ClaudeEventNormalizer::default();
        let uuid = prompt_sent(&mut normalizer, expect_replay);

        assert!(matches!(
            normalize(
                &mut normalizer,
                json!({"type": "system", "subtype": "init", "session_id": "s1"})
            )
            .as_slice(),
            [NormalizedEvent::SessionStarted { .. }]
        ));
        // The orphaned notification is echoed under another uuid.
        assert!(
            normalize(
                &mut normalizer,
                json!({"type": "user", "isReplay": true, "uuid": "orphan", "message": {"role": "user", "content": "<task-notification>\n<task-id>a1</task-id>\n<status>stopped</status>\n</task-notification>"}}),
            )
            .is_empty()
        );
        for result in [
            json!({"type": "result", "subtype": "success", "is_error": false, "usage": {"input_tokens": 0, "output_tokens": 0}}),
            json!({"type": "result", "subtype": "success", "result": ""}),
            json!({"type": "result", "subtype": "success", "result": " \n"}),
        ] {
            // Not even its token usage: that turn was not this one.
            let events = normalize(&mut normalizer, result);
            assert!(events.is_empty(), "{events:?}");
            // Bounded, so a prompt that is never taken in fails the turn.
            assert_eq!(
                normalizer.turn_hold(),
                TurnHold::Idle(FOLLOW_UP_IDLE_TIMEOUT)
            );
        }

        assert!(normalize(&mut normalizer, replay(&uuid)).is_empty());
        assert_eq!(normalizer.turn_hold(), TurnHold::Released);
        normalize(&mut normalizer, message_start("msg_1"));
        let events = normalize(&mut normalizer, answer("msg_1", "Here is what changed."));
        assert_eq!(
            agent_texts(&events),
            vec![(
                Some("end_turn".to_string()),
                "Here is what changed.".to_string()
            )]
        );
        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "Here is what changed."}),
        );
        assert!(events.iter().any(NormalizedEvent::is_terminal));
        assert_eq!(normalizer.turn_hold(), TurnHold::Released);
    }

    #[test]
    fn model_output_takes_the_prompt_as_read_for_a_cli_that_does_not_echo_it() {
        for first in [
            message_start("msg_1"),
            json!({"type": "stream_event", "event": {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}}),
            answer("msg_1", "done"),
            json!({"type": "assistant", "is_partial": true, "message": {"id": "msg_1", "content": [{"type": "text", "text": "do"}]}}),
            json!({"type": "assistant", "message": {"id": "msg_1", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}]}}),
        ] {
            let mut normalizer = ClaudeEventNormalizer::default();
            prompt(&mut normalizer);
            normalize(&mut normalizer, first.clone());
            assert_eq!(normalizer.pending_turn_prompt, None, "{first}");
            // Even a `result` without text now ends the turn.
            let events = normalize(
                &mut normalizer,
                json!({"type": "result", "subtype": "success"}),
            );
            assert!(events.iter().any(NormalizedEvent::is_terminal), "{first}");
            assert_eq!(normalizer.turn_hold(), TurnHold::Released);
        }
    }

    #[test]
    fn nothing_before_the_echo_answers_a_prompt_that_claude_code_echoes() {
        let stale = [
            // The model's own output, cut off with the previous process or
            // answering a prompt that process never finished.
            answer_stream("msg_stale", "A stale answer."),
            vec![answer("msg_stale", "A stale answer.")],
            vec![
                json!({"type": "assistant", "is_partial": true, "message": {"id": "msg_stale", "content": [{"type": "text", "text": "A stale"}]}}),
            ],
            vec![
                json!({"type": "assistant", "message": {"id": "msg_stale", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}]}}),
                json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "stale output"}]}}),
            ],
            // A delta with no message around it.
            vec![stream(
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "stale"}}),
            )],
            // Work that died with the previous process.
            vec![
                json!({"type": "system", "subtype": "task_started", "task_id": "a1", "task_type": "local_agent", "is_backgrounded": true}),
                json!({"type": "system", "subtype": "task_notification", "task_id": "a1", "status": "stopped", "summary": "stale"}),
            ],
            synthetic_stream("No response requested."),
        ];
        for events in stale {
            for result in [
                json!({"type": "result", "subtype": "success"}),
                json!({"type": "result", "subtype": "success", "result": "A stale answer.", "usage": {"input_tokens": 9, "output_tokens": 9}}),
            ] {
                let mut normalizer = ClaudeEventNormalizer::default();
                let uuid = echoed_prompt(&mut normalizer);

                let out = normalize_all(&mut normalizer, events.clone());
                assert!(out.is_empty(), "{events:?}: {out:?}");
                assert_eq!(normalizer.pending_turn_prompt.as_deref(), Some(&*uuid));
                // Nothing dropped holds the turn or arms the stop fallback.
                assert_eq!(normalizer.turn_hold(), TurnHold::Released);

                // The `result` of that turn is not ours, even with text.
                let out = normalize(&mut normalizer, result.clone());
                assert!(out.is_empty(), "{result}: {out:?}");
                assert_eq!(normalizer.pending_turn_prompt.as_deref(), Some(&*uuid));
                assert_eq!(
                    normalizer.turn_hold(),
                    TurnHold::Idle(FOLLOW_UP_IDLE_TIMEOUT)
                );

                // The echo of another message is not the prompt's.
                assert!(normalize(&mut normalizer, replay("another-uuid")).is_empty());
                assert_eq!(normalizer.pending_turn_prompt.as_deref(), Some(&*uuid));
                let out = normalize_all(&mut normalizer, answer_stream("msg_late", "Still stale."));
                assert!(out.is_empty(), "{out:?}");

                assert!(normalize(&mut normalizer, replay(&uuid)).is_empty());
                assert_eq!(normalizer.pending_turn_prompt, None);
                assert_eq!(normalizer.turn_hold(), TurnHold::Released);
                let mut out =
                    normalize_all(&mut normalizer, answer_stream("msg_1", "The real answer."));
                out.extend(normalize(
                    &mut normalizer,
                    json!({"type": "result", "subtype": "success", "result": "The real answer."}),
                ));
                assert_eq!(
                    agent_texts(&out),
                    vec![(Some("end_turn".to_string()), "The real answer.".to_string())]
                );
                assert_eq!(
                    out.iter().filter(|event| event.is_terminal()).count(),
                    1,
                    "{out:?}"
                );
                assert_eq!(normalizer.turn_hold(), TurnHold::Released);
            }
        }
    }

    #[test]
    fn the_session_id_and_a_failure_get_through_before_the_echo() {
        let mut normalizer = ClaudeEventNormalizer::default();
        echoed_prompt(&mut normalizer);
        assert!(matches!(
            normalize(
                &mut normalizer,
                json!({"type": "system", "subtype": "init", "session_id": "s1"})
            )
            .as_slice(),
            [NormalizedEvent::SessionStarted { session_id: Some(session_id) }] if session_id == "s1"
        ));

        // Claude Code that cannot start or authenticate echoes nothing: the
        // turn must fail on what it reports instead of waiting for an echo.
        for (failure, expected) in [
            (
                json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "result": "Invalid API key"}),
                "Invalid API key",
            ),
            (
                json!({"type": "result", "subtype": "success", "is_error": true, "result": "API Error: 529 Overloaded"}),
                "API Error: 529 Overloaded",
            ),
            (json!({"type": "error", "message": "boom"}), "boom"),
        ] {
            let mut normalizer = ClaudeEventNormalizer::default();
            echoed_prompt(&mut normalizer);
            let out = normalize(&mut normalizer, failure.clone());
            assert!(
                matches!(
                    out.last(),
                    Some(NormalizedEvent::Result { error: Some(error) } | NormalizedEvent::Error { message: error })
                        if error == expected
                ),
                "{failure}: {out:?}"
            );
            assert_eq!(normalizer.turn_hold(), TurnHold::Released);
        }
    }

    #[test]
    fn claude_codes_own_stream_does_not_take_the_prompt_as_read_for_a_cli_that_does_not_echo_it() {
        for result in [
            json!({"type": "result", "subtype": "success"}),
            json!({"type": "result", "subtype": "success", "result": "No response requested."}),
        ] {
            let mut normalizer = ClaudeEventNormalizer::default();
            let uuid = prompt(&mut normalizer);

            // Each event alone must leave the prompt pending: read as model
            // output, the `message_start` used to release the turn, which
            // then ended on this message's `result`.
            for event in synthetic_stream("No response requested.") {
                let out = normalize(&mut normalizer, event.clone());
                assert!(out.is_empty(), "{event}: {out:?}");
                assert_eq!(
                    normalizer.pending_turn_prompt.as_deref(),
                    Some(&*uuid),
                    "{event}"
                );
                assert_eq!(normalizer.turn_hold(), TurnHold::Released, "{event}");
            }
            let out = normalize(&mut normalizer, result.clone());
            assert!(out.is_empty(), "{result}: {out:?}");
            assert_eq!(
                normalizer.turn_hold(),
                TurnHold::Idle(FOLLOW_UP_IDLE_TIMEOUT)
            );
            assert!(normalize(&mut normalizer, replay("another-uuid")).is_empty());
            assert_eq!(normalizer.pending_turn_prompt.as_deref(), Some(&*uuid));

            // The model's own message is what takes the prompt in here.
            let mut out =
                normalize_all(&mut normalizer, answer_stream("msg_1", "The real answer."));
            assert_eq!(normalizer.pending_turn_prompt, None);
            out.extend(normalize(
                &mut normalizer,
                json!({"type": "result", "subtype": "success", "result": "The real answer."}),
            ));
            assert_eq!(
                agent_texts(&out),
                vec![(Some("end_turn".to_string()), "The real answer.".to_string())]
            );
            assert_eq!(out.iter().filter(|event| event.is_terminal()).count(), 1);
        }
    }

    #[test]
    fn claude_codes_own_stream_after_the_prompt_is_taken_in_is_delivered() {
        for expect_replay in [false, true] {
            let mut normalizer = ClaudeEventNormalizer::default();
            let uuid = prompt_sent(&mut normalizer, expect_replay);
            normalize(&mut normalizer, replay(&uuid));

            let out = normalize_all(
                &mut normalizer,
                synthetic_stream("API Error: 529 Overloaded"),
            );
            assert_eq!(
                agent_texts(&out),
                vec![(
                    Some("stop_sequence".to_string()),
                    "API Error: 529 Overloaded".to_string()
                )]
            );
        }
    }

    #[test]
    fn claude_codes_own_message_before_the_prompt_is_taken_in_is_not_the_answer() {
        for expect_replay in [false, true] {
            own_message_before_the_prompt_is_not_the_answer(expect_replay);
        }
    }

    fn own_message_before_the_prompt_is_not_the_answer(expect_replay: bool) {
        let mut normalizer = ClaudeEventNormalizer::default();
        let uuid = prompt_sent(&mut normalizer, expect_replay);

        // Shown as the answer, its terminal stop would also complete the
        // turn by the stop fallback.
        assert!(normalize(&mut normalizer, synthetic("No response requested.")).is_empty());
        assert_eq!(normalizer.pending_turn_prompt.as_deref(), Some(&*uuid));
        // The `result` repeats that message's text and is still not ours.
        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "No response requested."}),
        );
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(
            normalizer.turn_hold(),
            TurnHold::Idle(FOLLOW_UP_IDLE_TIMEOUT)
        );

        normalize(&mut normalizer, replay(&uuid));
        normalize(&mut normalizer, message_start("msg_1"));
        let mut events = normalize(&mut normalizer, answer("msg_1", "The real answer."));
        events.extend(normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "The real answer."}),
        ));
        assert_eq!(
            agent_texts(&events),
            vec![(Some("end_turn".to_string()), "The real answer.".to_string())]
        );
        assert!(events.iter().any(NormalizedEvent::is_terminal));
    }

    #[test]
    fn claude_codes_own_message_after_the_prompt_is_taken_in_is_delivered() {
        for expect_replay in [false, true] {
            own_message_after_the_prompt_is_delivered(expect_replay);
        }
    }

    fn own_message_after_the_prompt_is_delivered(expect_replay: bool) {
        let mut normalizer = ClaudeEventNormalizer::default();
        let uuid = prompt_sent(&mut normalizer, expect_replay);
        normalize(&mut normalizer, replay(&uuid));

        // An API failure is reported this way, with an error `result`.
        let events = normalize(&mut normalizer, synthetic("API Error: 529 Overloaded"));
        assert_eq!(
            agent_texts(&events),
            vec![(
                Some("stop_sequence".to_string()),
                "API Error: 529 Overloaded".to_string()
            )]
        );
        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "is_error": true, "result": "API Error: 529 Overloaded"}),
        );
        assert!(matches!(
            events.last(),
            Some(NormalizedEvent::Result { error: Some(error) }) if error == "API Error: 529 Overloaded"
        ));
    }

    #[test]
    fn result_with_text_and_no_message_ends_the_turn_of_a_cli_that_reports_only_that() {
        let mut normalizer = ClaudeEventNormalizer::default();
        prompt(&mut normalizer);
        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "final answer"}),
        );
        assert!(events.iter().any(NormalizedEvent::is_terminal));
        // Nothing is held, so the process is kept for the next turn.
        assert_eq!(normalizer.turn_hold(), TurnHold::Released);
    }

    #[test]
    fn failed_result_before_the_prompt_is_taken_in_ends_the_turn() {
        let failed =
            json!({"type": "result", "subtype": "error_during_execution", "is_error": true});

        for expect_replay in [false, true] {
            let mut normalizer = ClaudeEventNormalizer::default();
            prompt_sent(&mut normalizer, expect_replay);
            let events = normalize(&mut normalizer, failed.clone());
            assert!(events.iter().any(NormalizedEvent::is_terminal));
            assert_eq!(normalizer.turn_hold(), TurnHold::Released);

            // After a swallowed `result` the prompt is still queued in Claude
            // Code, so the server stops the process with the turn.
            let mut normalizer = ClaudeEventNormalizer::default();
            prompt_sent(&mut normalizer, expect_replay);
            normalize(
                &mut normalizer,
                json!({"type": "result", "subtype": "success"}),
            );
            let events = normalize(&mut normalizer, failed.clone());
            assert!(events.iter().any(NormalizedEvent::is_terminal));
            assert_ne!(normalizer.turn_hold(), TurnHold::Released);
        }
    }

    #[test]
    fn a_new_prompt_starts_its_own_wait() {
        let mut normalizer = ClaudeEventNormalizer::default();
        prompt(&mut normalizer);
        normalize(&mut normalizer, synthetic("No response requested."));
        normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success"}),
        );
        assert_ne!(normalizer.turn_hold(), TurnHold::Released);

        // A held steer continues the turn as the next prompt.
        prompt(&mut normalizer);
        assert_eq!(normalizer.turn_hold(), TurnHold::Released);
        assert!(!normalizer.pre_prompt_message);
        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "only a result"}),
        );
        assert!(events.iter().any(NormalizedEvent::is_terminal));
        assert_eq!(normalizer.turn_hold(), TurnHold::Released);
    }

    #[test]
    fn task_notification_user_message_does_not_settle_the_answer_as_commentary() {
        let mut normalizer = ClaudeEventNormalizer::default();
        normalize(
            &mut normalizer,
            json!({"type": "assistant", "message": {"id": "msg_1", "stop_reason": null, "content": [{"type": "text", "text": "final answer"}]}}),
        );
        // Not a replay and no tool result: nothing the answer waits on.
        let events = normalize(
            &mut normalizer,
            json!({"type": "user", "message": {"role": "user", "content": "<task-notification>\n<task-id>a1</task-id>\n</task-notification>"}}),
        );
        assert!(events.is_empty(), "{events:?}");

        let events = normalize(
            &mut normalizer,
            json!({"type": "result", "subtype": "success", "result": "final answer"}),
        );
        assert_eq!(
            agent_texts(&events),
            vec![(Some("end_turn".to_string()), "final answer".to_string())]
        );
    }

    #[test]
    fn normalizes_claude_code_effort_levels() {
        assert_eq!(
            ClaudeCodeHarness.reasoning_effort(" High ").as_deref(),
            Some("high")
        );
        assert_eq!(
            ClaudeCodeHarness.reasoning_effort("max").as_deref(),
            Some("max")
        );
        for effort in ["minimal", "none", "ultra"] {
            assert_eq!(ClaudeCodeHarness.reasoning_effort(effort), None, "{effort}");
        }
    }

    #[test]
    fn effort_is_applied_with_a_flag_settings_control_request() {
        for (effort, level) in [(Some("max"), json!("max")), (None, Value::Null)] {
            let bytes = ClaudeCodeHarness
                .stdin_for_reasoning_effort(effort)
                .unwrap();
            assert_eq!(bytes.last(), Some(&b'\n'));
            let value: Value = serde_json::from_slice(&bytes).unwrap();

            assert_eq!(value["type"], "control_request");
            assert!(value["request_id"].as_str().unwrap().starts_with("effort-"));
            assert_eq!(
                value["request"],
                json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": level}})
            );
        }
    }
}

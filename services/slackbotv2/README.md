# Slackbot v2

## Claude prompt suggestions

Set `SLACKBOTV2_PROMPT_SUGGESTIONS_ENABLED=true` to opt in to Claude Code's
native next-prompt suggestions. It defaults to false and requires
`SLACKBOTV2_AGENT_VIEW_ENABLED=true` plus Slack's `assistant:write` scope.
Remove any static suggested prompts from the Slack app configuration, since
Slack rejects dynamic updates when static prompts are configured.
Only Claude executions in direct messages request suggestions; other harnesses
and channel conversations keep their existing behavior.

Claude may return one suggestion after its answer, or none. It uses a separate
background model request with the current conversation, so enabling this feature
adds model usage. Centaur does not generate replacement suggestions. The answer
streams immediately; optional suggestions are collected for a bounded period.

Slack's native `assistant.threads.setSuggestedPrompts` feature displays these
at the top of the Agent view's Messages tab. Clicking one follows Slack's normal
message flow and does not guarantee continuation in the original thread.
Centaur clears its prior suggestions when a newer DM message is accepted and
ignores suggestions from older executions. Suggested-prompt writes are
best-effort and cannot fail answer delivery.

References: [Claude prompt suggestions](https://code.claude.com/docs/en/interactive-mode#prompt-suggestions),
[Slack suggested prompts](https://docs.slack.dev/reference/methods/assistant.threads.setSuggestedPrompts/).

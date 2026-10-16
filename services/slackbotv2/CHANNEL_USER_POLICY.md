# Channel user admission

Set `SLACKBOTV2_CHANNEL_USER_POLICY` to a JSON object keyed by protected Slack
channel ID. Each entry has `team_id` and a nonempty `allowed_user_ids` list:

```json
{"CEXAMPLE":{"team_id":"TEXAMPLE","allowed_user_ids":["UEXAMPLE"]}}
```

Malformed configuration fails startup. Unlisted channels keep normal admission.
In protected channels, only listed humans can mention, follow up, steer, stop,
or invoke channel-bound controls. Bot messages and message edits cannot trigger
execution there. Rejected events receive an HTTP acknowledgement to avoid Slack
retries, but never reach session creation, append, execution or late-file repair.
The SDK still verifies Slack signatures for accepted requests. The message
callback also checks the normalized actor before handoff.

This policy controls Slack ingress, not direct API calls, Console access or
provider workflows. Those need their own authorization; a signed provider
webhook may deliberately start an automated session without granting it AWS
access. Any separate command/interaction receiver must enforce the same policy.

Sandbox reuse is unchanged. Before granting a sensitive capability, retire or
exclude sessions created before this boundary was enabled: changing admission
does not remove earlier processes or files. Channel membership and session
history are not authorization. Thread context can still contain untrusted
content; this gate does not prevent prompt injection during an approved turn.

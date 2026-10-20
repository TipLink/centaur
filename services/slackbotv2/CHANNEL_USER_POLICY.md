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

## Fixed thread ownership

Set `SLACKBOTV2_THREAD_ADMISSION_URL` to the deployment's trusted admission
service. For channels in the user policy, Slackbot forwards the original signed
message bytes before any SDK callback, late-file repair, append, stop or steering.
Only an explicit `{ "allowed": true }` admits a message. Timeouts, errors and
malformed replies fail closed; other channels do not call this service.
Protected-channel buttons are disabled while this policy is enabled.

The service must verify Slack's signature itself and atomically reserve a fixed
thread owner before the first session starts. It must refuse ownership changes
and pre-existing unowned sessions. Provider workflows must reserve the same
owner before starting their initial execution. The gateway must authorize that
fixed owner from the first execution onward, including provider startup.
The service URL is trusted deployment configuration, never supplied by Slack.

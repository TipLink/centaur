import type { JsonObject, SlackbotV2BlockActionPayload, SlackbotV2Options } from './types'

export const TOOL_APPROVAL_ACTION_PREFIX = 'centaur.tool-approval:'

export type ApprovalDelivery = {
  id: string
  token: string
  revision: number
  action: string
  status: string
  payload_json: string
  payload_hash: string
  channel_id: string
  thread_ts: string
  requester_id: string
  decided_by: string | null
  message_ts: string | null
  expires_at: string
}

async function api(options: SlackbotV2Options, path: string, body: unknown): Promise<Response> {
  return (options.fetch ?? fetch)(new URL(path, options.apiUrl), {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${options.apiKey ?? process.env.SLACKBOT_API_KEY ?? ''}` },
    body: JSON.stringify(body), signal: AbortSignal.timeout(2000), redirect: 'error'
  })
}

export async function dispatchToolApproval(options: SlackbotV2Options, payload: SlackbotV2BlockActionPayload): Promise<JsonObject> {
  const match = payload.action_id.slice(TOOL_APPROVAL_ACTION_PREFIX.length)
    .match(/^([0-9a-f-]{36}):(approved|declined)$/)
  if (!match || !payload.team_id || !payload.channel_id || !payload.message_ts
    || !payload.user_id || !/^[a-f0-9]{64}$/.test(payload.value ?? '')) {
    throw new Error('Invalid tool approval click')
  }
  const response = await api(options, `/api/tool-approvals/${match[1]}/decide`, {
    team_id: payload.team_id, channel_id: payload.channel_id, message_ts: payload.message_ts,
    user_id: payload.user_id, payload_hash: payload.value, decision: match[2],
    message_blocks: payload.approval_message_blocks ?? null
  })
  if (response.status === 403) {
    await response.body?.cancel()
    return { outcome: 'unavailable' }
  }
  if (!response.ok) throw new Error(`Tool approval handoff failed (${response.status})`)
  return { outcome: 'accepted' }
}

export function approvalMessage(delivery: ApprovalDelivery): JsonObject {
  // Render the broker's frozen serialization verbatim. Parsing/re-serializing
  // here would round JSON integers outside JavaScript's safe integer range.
  const fullPayload = delivery.payload_json
  if (Buffer.byteLength(fullPayload) > 12000) throw new Error('Approval payload exceeds display limit')
  const text = `Tool approval: ${delivery.action} — ${delivery.status}`
  const blocks: JsonObject[] = [
    { type: 'header', text: { type: 'plain_text', text } },
    { type: 'section', text: { type: 'plain_text', text: `Requested by ${delivery.requester_id}. Expires ${delivery.expires_at}.\nReview the complete tool arguments below before accepting.` } }
  ]
  // Plaintext prevents arguments from injecting mentions, links or Block Kit.
  // Split Unicode code points, not UTF-16 pairs. Nothing is truncated.
  const points = Array.from(fullPayload)
  for (let i = 0; i < points.length; i += 1400) {
    blocks.push({ type: 'section', expand: true, text: { type: 'plain_text', text: points.slice(i, i + 1400).join(''), emoji: false } })
  }
  blocks.push({ type: 'context', elements: [{ type: 'plain_text', text: `Payload SHA-256: ${delivery.payload_hash}` }] })
  if (delivery.status === 'pending') {
    blocks.push({ type: 'actions', elements: [
      { type: 'button', text: { type: 'plain_text', text: 'Accept' }, style: 'primary',
        action_id: `${TOOL_APPROVAL_ACTION_PREFIX}${delivery.id}:approved`, value: delivery.payload_hash },
      { type: 'button', text: { type: 'plain_text', text: 'Decline' }, style: 'danger',
        action_id: `${TOOL_APPROVAL_ACTION_PREFIX}${delivery.id}:declined`, value: delivery.payload_hash }
    ] })
  } else {
    const explanation = delivery.status === 'unknown'
      ? 'Outcome uncertain. Inspect the provider before requesting this action again.'
      : `Status: ${delivery.status}${delivery.decided_by ? ` · Decision by ${delivery.decided_by}` : ''}`
    blocks.push({ type: 'section', text: { type: 'plain_text', text: explanation } })
  }
  return { text, blocks, unfurl_links: false, unfurl_media: false }
}

export async function deliverToolApprovals(options: SlackbotV2Options): Promise<void> {
  if (!options.slackHomeTeamId) throw new Error('Tool approval delivery requires a verified Slack team')
  for (let i = 0; i < 20; i++) {
    const response = await api(options, '/api/tool-approvals/delivery/claim', { team_id: options.slackHomeTeamId })
    if (!response.ok) throw new Error(`Tool approval delivery claim failed (${response.status})`)
    const { delivery } = await response.json() as { delivery: ApprovalDelivery | null }
    if (!delivery) return
    const method = delivery.message_ts ? 'chat.update' : 'chat.postMessage'
    const message = approvalMessage(delivery)
    const slack = await (options.fetch ?? fetch)(`${(options.slackApiUrl ?? 'https://slack.com/api').replace(/\/$/, '')}/${method}`, {
      method: 'POST', headers: { authorization: `Bearer ${options.botToken}`, 'content-type': 'application/json' },
      body: JSON.stringify({ ...message, channel: delivery.channel_id,
        ...(delivery.message_ts ? { ts: delivery.message_ts } : { thread_ts: delivery.thread_ts, client_msg_id: delivery.id }) }),
      signal: AbortSignal.timeout(5000), redirect: 'error'
    })
    const result = await slack.json() as { ok?: boolean, ts?: string }
    if (!slack.ok || !result.ok || !result.ts) throw new Error('Tool approval Slack delivery failed')
    const ack = await api(options, `/api/tool-approvals/${delivery.id}/delivered`, {
      token: delivery.token, revision: delivery.revision, message_ts: result.ts, message_blocks: message.blocks
    })
    if (!ack.ok) throw new Error(`Tool approval delivery receipt failed (${ack.status})`)
  }
}

export function startToolApprovalDelivery(options: SlackbotV2Options): () => void {
  let active = false
  const timer = setInterval(() => {
    if (active) return
    active = true
    void deliverToolApprovals(options).catch(() => {
      options.logger?.warn('slackbotv2_tool_approval_delivery_failed')
    }).finally(() => { active = false })
  }, 2000)
  return () => clearInterval(timer)
}

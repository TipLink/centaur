import { expect, test } from 'bun:test'
import { createHmac } from 'node:crypto'
import { createMemoryState } from '@chat-adapter/state-memory'
import { createSlackbotV2 } from '../src/index'
import { approvalMessage, deliverToolApprovals, type ApprovalDelivery } from '../src/tool-approvals'
import type { JsonObject, SlackbotV2Options } from '../src/types'

const delivery: ApprovalDelivery = {
  id: '00000000-0000-4000-8000-000000000001', token: '00000000-0000-4000-8000-000000000002',
  revision: 1, action: 'example-create', status: 'pending', payload_json: '{"amount":9007199254740993,"body":"<!channel>"}',
  payload_hash: 'a'.repeat(64), channel_id: 'CTEST', thread_ts: '1.000', requester_id: 'UREQUESTER',
  decided_by: null, message_ts: null, expires_at: '2026-09-29T12:00:00Z'
}
const options: SlackbotV2Options = {
  apiUrl: 'https://api.test', apiKey: 'test-ingress-key', botToken: 'test-bot-token',
  botUserId: 'UBOT', slackHomeTeamId: 'TTEST', slackApiUrl: 'https://slack.test/api', signingSecret: 'test-signing-secret'
}

test('renders every payload byte as plaintext, preserving large integers and Unicode', () => {
  const d = { ...delivery, payload_json: '{"body":"' + '😀<!channel>'.repeat(400) + '"}' }
  const message = approvalMessage(d)
  const blocks = message.blocks as JsonObject[]
  const payload = blocks.filter(block => block.expand === true)
  expect(payload.map(block => (block.text as JsonObject).text).join('')).toBe(d.payload_json)
  expect(payload.every(block => (block.text as JsonObject).type === 'plain_text')).toBe(true)
  expect(payload.every(block => String((block.text as JsonObject).text).length <= 3000)).toBe(true)
  expect(JSON.stringify(approvalMessage(delivery))).toContain('9007199254740993')
  expect(() => approvalMessage({ ...delivery, payload_json: 'x'.repeat(12001) })).toThrow()
  expect((approvalMessage({ ...delivery, status: 'declined' }).blocks as JsonObject[]).some(block => block.type === 'actions')).toBe(false)
})

test('delivery posts in the originating thread and records the exact displayed card', async () => {
  let claims = 0
  let posted: JsonObject | undefined
  let receipt: JsonObject | undefined
  await deliverToolApprovals({ ...options, fetch: async (url, init) => {
    const path = new URL(String(url)).pathname
    const body = JSON.parse(String(init?.body))
    if (path.endsWith('/claim')) return Response.json({ delivery: claims++ === 0 ? delivery : null })
    if (path.endsWith('/chat.postMessage')) { posted = body; return Response.json({ ok: true, ts: '2.000' }) }
    if (path.endsWith('/delivered')) { receipt = body; return Response.json({ ok: true }) }
    throw new Error(`Unexpected request ${path}`)
  } })
  expect(posted?.channel).toBe('CTEST')
  expect(posted?.thread_ts).toBe('1.000')
  expect(posted?.client_msg_id).toBe(delivery.id)
  expect(receipt?.message_blocks).toEqual(posted?.blocks)
  expect(receipt?.message_ts).toBe('2.000')
})

test('signed SDK clicks await durable handoff, reject bad signatures, and preserve card and actor', async () => {
  const requests: JsonObject[] = []
  let fail = true
  const bot = createSlackbotV2({ ...options, state: createMemoryState(), fetch: async (url, init) => {
    if (String(url).endsWith('/decide')) {
      requests.push(JSON.parse(String(init?.body)))
      return fail ? new Response('', { status: 503 }) : Response.json({ outcome: 'accepted' })
    }
    return Response.json({ ok: true, team_id: 'TTEST', user_id: 'UBOT', bot_id: 'BTEST' })
  } })
  const message = approvalMessage(delivery)
  const payload = {
    type: 'block_actions', team: { id: 'TTEST' }, user: { id: 'UALICE', username: 'alice', team_id: 'TTEST' },
    channel: { id: 'CTEST' }, message: { ts: '2.000', ...message },
    actions: [{ type: 'button', action_id: `centaur.tool-approval:${delivery.id}:approved`, value: delivery.payload_hash, action_ts: '3.000' }]
  }
  const signed = (valid: boolean): RequestInit => {
    const timestamp = String(Math.floor(Date.now() / 1000))
    const body = new URLSearchParams({ payload: JSON.stringify(payload) }).toString()
    const mac = createHmac('sha256', 'test-signing-secret').update(`v0:${timestamp}:${body}`).digest('hex')
    return { method: 'POST', body, headers: { 'content-type': 'application/x-www-form-urlencoded',
      'x-slack-request-timestamp': timestamp, 'x-slack-signature': valid ? `v0=${mac}` : 'v0=invalid' } }
  }
  const waits: Promise<unknown>[] = []
  const ctx = { waitUntil: (p: Promise<unknown>) => { waits.push(p) }, passThroughOnException() {}, props: {} }
  expect((await bot.app.request('/api/slack/actions', signed(false), {}, ctx)).status).not.toBe(200)
  expect(requests).toHaveLength(0)
  expect((await bot.app.request('/api/slack/actions', signed(true), {}, ctx)).status).toBe(503)
  fail = false
  expect((await bot.app.request('/api/slack/actions', signed(true), {}, ctx)).status).toBe(200)
  expect((await bot.app.request('/api/slack/actions', signed(true), {}, ctx)).status).toBe(200)
  await Promise.allSettled(waits)
  expect(requests).toHaveLength(3)
  expect(requests[0]).toEqual({ team_id: 'TTEST', channel_id: 'CTEST', message_ts: '2.000', user_id: 'UALICE',
    payload_hash: delivery.payload_hash, decision: 'approved', message_blocks: message.blocks })
})

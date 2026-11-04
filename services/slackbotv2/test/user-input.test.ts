import { createHmac } from 'node:crypto'
import { describe, expect, test } from 'bun:test'
import { createMemoryState } from '@chat-adapter/state-memory'
import type { Thread } from 'chat'
import { createSlackbotV2 } from '../src/index'
import { openSessionEventStream } from '../src/session-api'
import { renderSessionUserInput } from '../src/user-input'
import type { JsonObject, SessionUserInputAnswers, SessionUserInputRequest, SlackbotV2Options } from '../src/types'

const REQUEST = '11111111-1111-4111-8111-111111111111'
const THREAD = 'slack:C1:1700000000.000001'
const SECRET = 'test-question-signing-secret'
const requested = { eventKind: 'session.user_input.requested', data: { request_id: REQUEST } }

function harness(questionChanges: Partial<SessionUserInputRequest> = {}) {
  const state = createMemoryState()
  const request: SessionUserInputRequest = {
    request_id: REQUEST, execution_id: 'execution-1', requester_user_id: 'U1', status: 'pending',
    questions: [
      { id: 'project', header: 'Project', question: 'Which project fits?', options: [
        { label: 'Payments', description: 'Payment processing work' },
        { label: 'Platform', description: 'Shared platform work' }
      ] },
      { id: 'scope', header: 'Scope', question: 'Which scope should I use?', options: [
        { label: 'Small', description: 'One service' }, { label: 'Large', description: 'All services' }
      ] }
    ], ...questionChanges
  }
  const posts: JsonObject[] = []
  const updates: JsonObject[] = []
  const views: JsonObject[] = []
  const submissions: JsonObject[] = []
  const calls: string[] = []
  let failSubmitOnce = false
  let failWriteOnce = false
  let failPostOnce = false
  let failUpdateOnce = false
  let submitGate: Promise<void> | undefined
  let updateGate: Promise<void> | undefined
  const options: SlackbotV2Options = {
    apiUrl: 'http://session.test', slackApiUrl: 'http://slack.test/api/', state,
    botToken: 'xoxb-test-only', botUserId: 'UBOT', signingSecret: SECRET,
    recoverRenderObligationsOnStart: false,
    fetch: async (input, init) => {
      const url = new URL(String(input))
      calls.push(`${init?.method ?? 'GET'} ${url.pathname}`)
      if (url.hostname === 'session.test') {
        if (!url.pathname.endsWith(`/user-input/${REQUEST}`)) throw new Error(`Unexpected session request ${url}`)
        if (init?.method === 'POST') {
          const body: JsonObject = JSON.parse(String(init.body))
          await submitGate
          if (failSubmitOnce) { failSubmitOnce = false; return Response.json({ error: 'temporary' }, { status: 503 }) }
          request.answers = body.answers as SessionUserInputAnswers
          if (failWriteOnce) {
            failWriteOnce = false
            request.status = 'submitted'
            return Response.json({ error: 'write failed' }, { status: 503 })
          }
          submissions.push(body)
          request.status = 'resolved'
          return Response.json({ ok: true, status: 'resolved' })
        }
        return Response.json(request)
      }
      if (url.hostname !== 'slack.test') throw new Error('Unexpected external request')
      const form = new URLSearchParams(String(init?.body))
      const body = Object.fromEntries([...form].map(([key, value]) => {
        try { return [key, JSON.parse(value)] } catch { return [key, value] }
      })) as JsonObject
      const method = url.pathname.split('/').at(-1)
      if (method === 'chat.postMessage') {
        if (failPostOnce) { failPostOnce = false; throw new Error('connection failed before delivery') }
        posts.push({ ...body, user: 'UBOT', ts: '1700000001.000001' })
        return Response.json({ ok: true, ts: '1700000001.000001' })
      }
      if (method === 'chat.update') {
        await updateGate
        if (failUpdateOnce) { failUpdateOnce = false; return Response.json({ ok: false, error: 'temporary' }) }
        updates.push(body)
        return Response.json({ ok: true })
      }
      if (method === 'views.open') {
        views.push(body.view as JsonObject)
        return Response.json({ ok: true, view: { id: 'V1' } })
      }
      if (method === 'conversations.replies') return Response.json({ ok: true, messages: posts })
      throw new Error(`Unexpected Slack method ${method}`)
    }
  }
  const bot = createSlackbotV2(options)
  const render = async (event = requested) => {
    await state.connect()
    return renderSessionUserInput({ id: THREAD } as Thread, event, options)
  }
  const deliver = async (payload: JsonObject, signed = true, drain = true) => {
    const response = await bot.app.request('/api/slack/actions', signedRequest(payload, signed))
    if (drain) await bot.drain(5000)
    return response
  }
  const click = (index: number, choice: number | 'other' | 'edit', extra: JsonObject = {}, drain = true) => deliver({
    type: 'block_actions', team: { id: 'T1' }, user: { id: 'U1' }, channel: { id: 'C1' },
    message: { ts: '1700000001.000001', thread_ts: '1700000000.000001' }, trigger_id: 'trigger-1',
    actions: [{ action_id: `centaur.user_input:${choice}`, value: JSON.stringify({ request: REQUEST, index, choice }) }],
    ...extra
  }, true, drain)
  const submitModal = (value: string, extra: JsonObject = {}) => deliver({
    type: 'view_submission', team: { id: 'T1' }, user: { id: 'U1' },
    view: { ...views.at(-1), state: { values: { custom: { text: { value } } } } }, ...extra
  })
  return {
    state, request, options, bot, render, click, deliver, submitModal, posts, updates, views, submissions, calls,
    failSubmit: () => { failSubmitOnce = true }, failUpdate: () => { failUpdateOnce = true },
    failWrite: () => { failWriteOnce = true }, failPost: () => { failPostOnce = true },
    holdSubmit: (gate: Promise<void>) => { submitGate = gate },
    holdUpdate: (gate: Promise<void>) => { updateGate = gate }
  }
}

function signedRequest(payload: JsonObject, signed: boolean): RequestInit {
  const timestamp = Math.floor(Date.now() / 1000)
  const body = new URLSearchParams({ payload: JSON.stringify(payload) }).toString()
  return { method: 'POST', body, headers: {
    'content-type': 'application/x-www-form-urlencoded',
    'x-slack-request-timestamp': String(timestamp),
    'x-slack-signature': signed ? `v0=${createHmac('sha256', SECRET).update(`v0:${timestamp}:${body}`).digest('hex')}` : 'invalid'
  } }
}

describe('native questions in Slack', () => {
  test('signed buttons and typed answer collect a batch before resuming the original execution', async () => {
    const h = harness()
    await h.render()
    expect(h.posts).toHaveLength(1)
    expect(h.posts[0]!.text).toContain('Question 1 of 2')
    expect((await h.click(0, 1)).status).toBe(200)
    expect(h.submissions).toHaveLength(0)
    expect(h.updates.at(-1)!.text).toContain('Question 2 of 2')
    expect((await h.click(1, 'other')).status).toBe(200)
    expect(h.views.at(-1)!.title).toEqual({ type: 'plain_text', text: 'Question 2 of 2' })
    const response = await h.submitModal('Only the gateway service')
    expect(await response.json()).toEqual({ response_action: 'clear' })
    expect(h.submissions).toEqual([{
      execution_id: 'execution-1', responder_user_id: 'U1', answers: {
        project: { answers: ['Platform'] }, scope: { answers: ['Only the gateway service'] }
      }
    }])
    expect(h.updates.at(-1)!.text).toBe('Answers sent. Waiting for the task to continue.')
    await h.render({ eventKind: 'session.user_input.resolved', data: { request_id: REQUEST } })
    expect(h.updates.at(-1)!.text).toBe('Answers received. Continuing the task.')
    expect(h.calls.some(call => call.endsWith('/execute'))).toBe(false)
    await h.submitModal('Only the gateway service')
    expect(h.submissions).toHaveLength(1)
  })

  test('rejects unsigned, different requester, wrong message and old-page clicks', async () => {
    const h = harness()
    await h.render()
    const before = h.calls.length
    expect((await h.deliver({ type: 'block_actions' }, false)).status).toBe(401)
    expect(h.calls).toHaveLength(before)
    await h.click(0, 0, { user: { id: 'U2' } })
    await h.click(0, 0, { channel: { id: 'C2' } })
    await h.click(0, 0, { message: { ts: 'wrong', thread_ts: '1700000000.000001' } })
    expect(h.calls).toHaveLength(before)
    await h.click(0, 0)
    await h.click(0, 1)
    await h.click(1, 0)
    expect(h.submissions[0]!.answers).toEqual({ project: { answers: ['Payments'] }, scope: { answers: ['Small'] } })
  })

  test('replay reuses the card, and recovery finds an accepted post with an unsaved timestamp', async () => {
    const h = harness()
    await h.render()
    await h.render()
    expect(h.posts).toHaveLength(1)
    const record = await h.state.get<Record<string, unknown>>(`slackbotv2:user-input:${REQUEST}`)
    delete record!.messageTs
    await h.state.set(`slackbotv2:user-input:${REQUEST}`, record)
    await h.render()
    expect(h.posts).toHaveLength(1)
    expect((await h.state.get<Record<string, unknown>>(`slackbotv2:user-input:${REQUEST}`))!.messageTs).toBe('1700000001.000001')
    expect(h.calls.some(call => call.endsWith('conversations.replies'))).toBe(true)
  })

  test('ignores a stale requested event and closes a cancelled request', async () => {
    const h = harness()
    await h.render()
    h.request.status = 'cancelled'
    await h.render({ eventKind: 'session.user_input.resolved', data: { request_id: REQUEST } })
    expect(h.updates.at(-1)!.text).toContain('no longer waiting')
    await h.click(0, 0)
    expect(h.submissions).toHaveLength(0)
    const stale = harness({ status: 'resolved' })
    await stale.render()
    expect(stale.posts).toHaveLength(0)
  })

  test('reconciles an unknown post and retries the same message id without leaving the question stuck', async () => {
    const h = harness()
    h.failPost()
    await expect(h.render()).rejects.toThrow('connection failed before delivery')
    await expect(h.render()).rejects.toThrow('still unconfirmed')
    expect(h.posts).toHaveLength(0)
    const key = `slackbotv2:user-input:${REQUEST}`
    const card = await h.state.get<Record<string, unknown>>(key)
    card!.postingAt = Date.now() - 31_000
    await h.state.set(key, card)
    await h.render()
    expect(h.posts).toHaveLength(1)
    expect(h.posts[0]!.client_msg_id).toBe(REQUEST)
    await h.click(0, 0)
    await h.click(1, 0)
    expect(h.submissions).toHaveLength(1)
  })

  test('retries a saved final answer after transient API failure without changing earlier answers', async () => {
    const h = harness()
    await h.render()
    await h.click(0, 0)
    await h.click(1, 'other')
    h.failSubmit()
    expect((await (await h.submitModal('One service')).json()).response_action).toBe('errors')
    expect(h.submissions).toHaveLength(0)
    expect((await (await h.submitModal('One service')).json()).response_action).toBe('clear')
    expect(h.submissions).toHaveLength(1)
  })

  test('retries a failed page update after acknowledging the saved choice', async () => {
    const h = harness()
    await h.render()
    h.failUpdate()
    expect((await h.click(0, 0)).status).toBe(200)
    expect((await h.click(0, 0)).status).toBe(200)
    expect(h.updates.at(-1)!.text).toContain('Question 2 of 2')
    expect(h.submissions).toHaveLength(0)
  })

  test('acknowledges a saved choice before Slack finishes rendering it', async () => {
    const h = harness()
    await h.render()
    let release!: () => void
    h.holdUpdate(new Promise<void>(resolve => { release = resolve }))
    const started = Date.now()
    const response = await h.click(0, 0, {}, false)
    expect(response.status).toBe(200)
    expect(Date.now() - started).toBeLessThan(1000)
    expect(h.updates).toHaveLength(0)
    release()
    await h.bot.drain(5000)
    expect(h.updates.at(-1)!.text).toContain('Question 2 of 2')
  })

  test('returns a retryable response before Slack times out a slow durable handoff', async () => {
    const h = harness()
    await h.render()
    await h.click(0, 0)
    let release!: () => void
    h.holdSubmit(new Promise<void>(resolve => { release = resolve }))
    const started = Date.now()
    const response = await h.click(1, 0, {}, false)
    expect(response.status).toBe(503)
    expect(Date.now() - started).toBeLessThan(2800)
    expect(h.submissions).toHaveLength(0)
    release()
    await h.bot.drain(5000)
    await h.click(1, 0)
    expect(h.submissions).toHaveLength(1)
  })

  test('retries closing a resolved card without holding the answer stream', async () => {
    const h = harness()
    await h.render()
    h.request.status = 'resolved'
    h.failUpdate()
    const pending: Promise<unknown>[] = []
    const consumed = await renderSessionUserInput({ id: THREAD } as Thread,
      { eventKind: 'session.user_input.resolved', data: { request_id: REQUEST } }, h.options,
      task => pending.push(task))
    expect(consumed).toBe(true)
    await Promise.all(pending)
    expect(h.updates.at(-1)!.text).toContain('Continuing the task')
  })

  test('retries answers persisted by the API when writing to the harness failed', async () => {
    const h = harness()
    await h.render()
    await h.click(0, 0)
    h.failWrite()
    expect((await h.click(1, 0)).status).toBe(503)
    expect(h.request.status).toBe('submitted')
    expect(h.submissions).toHaveLength(0)
    expect((await h.click(1, 0)).status).toBe(200)
    expect(h.submissions).toHaveLength(1)
    expect(h.request.status).toBe('resolved')
  })

  test('multiple choices and custom text return native answer arrays', async () => {
    const h = harness({ questions: [{ id: 'targets', header: 'Targets', question: 'Which services?', multiSelect: true,
      options: [{ label: 'API', description: '' }, { label: 'Worker', description: '' }] }] })
    await h.render()
    await h.click(0, 'edit')
    const response = await h.deliver({
      type: 'view_submission', team: { id: 'T1' }, user: { id: 'U1' },
      view: { ...h.views.at(-1), state: { values: {
        choices: { selected: { selected_options: [{ value: '0' }, { value: '1' }] } },
        custom: { text: { value: 'Scheduler' } }
      } } }
    })
    expect((await response.json()).response_action).toBe('clear')
    expect(h.submissions[0]!.answers).toEqual({ targets: { answers: ['API', 'Worker', 'Scheduler'] } })
  })

  test('accepts the tenth native choice and ids that match object property names', async () => {
    const h = harness({ questions: [{ id: '__proto__', header: 'Target', question: 'Which target?',
      options: Array.from({ length: 10 }, (_, index) => ({ label: `Target ${index}`, description: '' })) }] })
    await h.render()
    expect((await h.click(0, 9)).status).toBe(200)
    expect(Object.keys(h.submissions[0]!.answers as JsonObject)).toEqual(['__proto__'])
    expect((h.submissions[0]!.answers as JsonObject).__proto__).toEqual({ answers: ['Target 9'] })
  })

  test('a modal from a different channel cannot answer an otherwise valid request', async () => {
    const h = harness()
    await h.render()
    await h.click(0, 'other')
    const view = h.views.at(-1)!
    const metadata = JSON.parse(String(view.private_metadata))
    metadata.channel_id = 'C_OTHER'
    const response = await h.deliver({ type: 'view_submission', team: { id: 'T1' }, user: { id: 'U1' },
      view: { ...view, private_metadata: JSON.stringify(metadata),
        state: { values: { custom: { text: { value: 'Wrong channel' } } } } } })
    expect((await response.json()).response_action).toBe('errors')
    expect(h.submissions).toHaveLength(0)
    expect(h.updates).toHaveLength(0)
  })

  test('preserves literal question content rather than interpreting Slack mentions', async () => {
    const h = harness({ questions: [{ id: 'q', header: 'Choice', question: 'Use <!channel> or <https://example.com|this>?', options: [] }] })
    await h.render()
    const blocks = h.posts[0]!.blocks as JsonObject[]
    expect(blocks[1]!.text).toEqual({ type: 'plain_text', text: 'Use <!channel> or <https://example.com|this>?' })
  })

  test('the SSE parser passes requested and resolved events to live and recovery renders', async () => {
    const h = harness()
    const stream = await openSessionEventStream({ ...h.options, fetch: async () => new Response([
      `id: 1\nevent: session.user_input.requested\ndata: ${JSON.stringify({ request_id: REQUEST })}\n\n`,
      `id: 2\nevent: session.user_input.resolved\ndata: ${JSON.stringify({ request_id: REQUEST })}\n\n`,
      'id: 3\nevent: session.execution_completed\ndata: {}\n\n'
    ].join(''), { headers: { 'content-type': 'text/event-stream' } }) }, {
      threadId: THREAD, afterEventId: 0, executionId: 'execution-1', onEventId: () => {}
    })
    const events: unknown[] = []
    for await (const event of stream) events.push(event)
    expect(events).toMatchObject([
      { eventKind: 'session.user_input.requested', data: { request_id: REQUEST } },
      { eventKind: 'session.user_input.resolved', data: { request_id: REQUEST } },
      { eventKind: 'session.execution_completed' }
    ])
  })
})

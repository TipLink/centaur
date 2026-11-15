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
  const act = (action: JsonObject, extra: JsonObject = {}, drain = true) => deliver({
    type: 'block_actions', team: { id: 'T1' }, user: { id: 'U1' }, channel: { id: 'C1' },
    message: { ts: '1700000001.000001', thread_ts: '1700000000.000001' }, trigger_id: 'trigger-1',
    actions: [action], ...extra
  }, true, drain)
  /** Clicks Submit with the form state Slack attaches to a message's block action. */
  const submit = (values: JsonObject, extra: JsonObject = {}, drain = true) => act(
    { action_id: 'centaur.user_input:submit', value: JSON.stringify({ request: REQUEST }) },
    { state: { values }, ...extra }, drain)
  return {
    state, request, options, bot, render, act, submit, deliver, posts, updates, submissions, calls,
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

const pick = (index: number, value: string): JsonObject => ({
  [`q${index}`]: { selected: { type: 'radio_buttons', selected_option: { value } } }
})
const write = (index: number, value: string | null): JsonObject => ({
  [`c${index}`]: { text: { type: 'plain_text_input', value } }
})
const block = (message: JsonObject, id: string) =>
  (message.blocks as JsonObject[]).find(candidate => candidate.block_id === id) as JsonObject | undefined
const buttons = (message: JsonObject) => (message.blocks as JsonObject[])
  .flatMap(candidate => candidate.type === 'actions' ? candidate.elements as JsonObject[] : [])

describe('native questions in Slack', () => {
  test('one form shows every question and resumes the original execution on submit', async () => {
    const h = harness()
    await h.render()
    expect(h.posts).toHaveLength(1)
    const card = h.posts[0]!
    expect(card.text).toContain('Answer these 2 questions to continue.')
    for (const [index, name] of [[0, 'Project'], [1, 'Scope']] as const) {
      expect(block(card, `q${index}`)).toMatchObject({
        type: 'input', label: { type: 'plain_text', text: name }, element: { type: 'radio_buttons', action_id: 'selected' }
      })
      expect(block(card, `c${index}`)).toMatchObject({
        type: 'input', optional: true, element: { type: 'plain_text_input', action_id: 'text' }
      })
    }
    expect((block(card, 'q0')!.element as JsonObject).options).toEqual([
      { text: { type: 'plain_text', text: 'Payments' }, description: { type: 'plain_text', text: 'Payment processing work' }, value: '0' },
      { text: { type: 'plain_text', text: 'Platform' }, description: { type: 'plain_text', text: 'Shared platform work' }, value: '1' }
    ])
    expect(buttons(card).map(button => button.action_id)).toEqual(['centaur.user_input:submit'])
    expect((await h.submit({ ...pick(0, '1'), ...write(0, null), ...write(1, 'Only the gateway service') })).status).toBe(200)
    expect(h.submissions).toEqual([{
      execution_id: 'execution-1', responder_user_id: 'U1', answers: {
        project: { answers: ['Platform'] }, scope: { answers: ['Only the gateway service'] }
      }
    }])
    expect(h.updates.at(-1)!.text).toBe('Answers sent. Waiting for the task to continue.')
    await h.render({ eventKind: 'session.user_input.resolved', data: { request_id: REQUEST } })
    expect(h.updates.at(-1)!.text).toBe('Answers received. Continuing the task.')
    expect(h.calls.some(call => call.endsWith('/execute'))).toBe(false)
    await h.submit({ ...pick(0, '0'), ...pick(1, '0') })
    expect(h.submissions).toHaveLength(1)
  })

  test('rejects unsigned, different requester and wrong message submissions', async () => {
    const h = harness()
    await h.render()
    const before = h.calls.length
    const answers = { ...pick(0, '0'), ...pick(1, '0') }
    expect((await h.deliver({ type: 'block_actions' }, false)).status).toBe(401)
    expect(h.calls).toHaveLength(before)
    await h.submit(answers, { user: { id: 'U2' } })
    await h.submit(answers, { channel: { id: 'C2' } })
    await h.submit(answers, { message: { ts: 'wrong', thread_ts: '1700000000.000001' } })
    expect(h.calls).toHaveLength(before)
    expect((await h.act({ action_id: 'centaur.user_input:submit', value: 'not json' }, { state: { values: answers } })).status).toBe(400)
    expect(h.submissions).toHaveLength(0)
    await h.submit(answers)
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
    await h.submit({ ...pick(0, '0'), ...pick(1, '0') })
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
    await h.submit({ ...pick(0, '0'), ...pick(1, '0') })
    expect(h.submissions).toHaveLength(1)
  })

  test('an incomplete submission keeps the entries and names what is still needed', async () => {
    const h = harness()
    await h.render()
    expect((await h.submit({ ...pick(0, '1'), ...write(1, '   ') })).status).toBe(200)
    expect(h.submissions).toHaveLength(0)
    const form = h.updates.at(-1)!
    expect((form.blocks as JsonObject[])[0]).toMatchObject({ text: { text: expect.stringContaining('Still needed: Scope.') } })
    expect((block(form, 'q0')!.element as JsonObject).initial_option).toMatchObject({ value: '1' })
    expect((block(form, 'q1')!.element as JsonObject).initial_option).toBeUndefined()
    expect(buttons(form)).toHaveLength(1)
    await h.submit({ ...pick(0, '1'), ...write(1, 'Two services') })
    expect(h.submissions[0]!.answers).toEqual({ project: { answers: ['Platform'] }, scope: { answers: ['Two services'] } })
    expect((await h.state.get<Record<string, unknown>>(`slackbotv2:user-input:${REQUEST}`))!.notice).toBeUndefined()
  })

  test('a typed answer replaces the selection for a single choice and pre-fills a re-rendered form', async () => {
    const h = harness()
    await h.render()
    await h.submit({ ...pick(0, '0'), ...write(0, ' Billing ') })
    const form = h.updates.at(-1)!
    expect((block(form, 'c0')!.element as JsonObject).initial_value).toBe('Billing')
    expect((block(form, 'q0')!.element as JsonObject).initial_option).toBeUndefined()
    await h.submit({ ...pick(0, '0'), ...write(0, 'Billing'), ...pick(1, '1') })
    expect(h.submissions[0]!.answers).toEqual({ project: { answers: ['Billing'] }, scope: { answers: ['Large'] } })
  })

  test('retries a submission after transient API failure', async () => {
    const h = harness()
    await h.render()
    const answers = { ...pick(0, '0'), ...write(1, 'One service') }
    h.failSubmit()
    expect((await h.submit(answers)).status).toBe(503)
    expect(h.submissions).toHaveLength(0)
    expect((await h.submit(answers)).status).toBe(200)
    expect(h.submissions).toHaveLength(1)
  })

  test('retries a failed card update after acknowledging the answers', async () => {
    const h = harness()
    await h.render()
    h.failUpdate()
    expect((await h.submit({ ...pick(0, '0'), ...pick(1, '0') })).status).toBe(200)
    expect(h.submissions).toHaveLength(1)
    expect(h.updates.at(-1)!.text).toContain('Answers sent')
  })

  test('acknowledges the answers before Slack finishes rendering them', async () => {
    const h = harness()
    await h.render()
    let release!: () => void
    h.holdUpdate(new Promise<void>(resolve => { release = resolve }))
    const started = Date.now()
    const response = await h.submit({ ...pick(0, '0'), ...pick(1, '0') }, {}, false)
    expect(response.status).toBe(200)
    expect(Date.now() - started).toBeLessThan(1000)
    expect(h.updates).toHaveLength(0)
    release()
    await h.bot.drain(5000)
    expect(h.updates.at(-1)!.text).toContain('Answers sent')
  })

  test('returns a retryable response before Slack times out a slow durable handoff', async () => {
    const h = harness()
    await h.render()
    const answers = { ...pick(0, '0'), ...pick(1, '0') }
    let release!: () => void
    h.holdSubmit(new Promise<void>(resolve => { release = resolve }))
    const started = Date.now()
    const response = await h.submit(answers, {}, false)
    expect(response.status).toBe(503)
    expect(Date.now() - started).toBeLessThan(2800)
    expect(h.submissions).toHaveLength(0)
    release()
    await h.bot.drain(5000)
    await h.submit(answers)
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
    h.failWrite()
    expect((await h.submit({ ...pick(0, '0'), ...pick(1, '0') })).status).toBe(503)
    expect(h.request.status).toBe('submitted')
    expect(h.submissions).toHaveLength(0)
    // The API already stored the first answer set; a changed form cannot replace it.
    expect((await h.submit({ ...pick(0, '1'), ...pick(1, '1') })).status).toBe(200)
    expect(h.submissions).toHaveLength(1)
    expect(h.submissions[0]!.answers).toEqual({ project: { answers: ['Payments'] }, scope: { answers: ['Small'] } })
    expect(h.request.status).toBe('resolved')
  })

  test('multiple choices and custom text return native answer arrays', async () => {
    const h = harness({ questions: [{ id: 'targets', header: 'Targets', question: 'Which services?', multiSelect: true,
      options: [{ label: 'API', description: '' }, { label: 'Worker', description: '' }] }] })
    await h.render()
    expect(h.posts[0]!.text).toContain('Answer this question to continue.')
    expect(block(h.posts[0]!, 'q0')!.element).toEqual({ type: 'checkboxes', action_id: 'selected', options: [
      { text: { type: 'plain_text', text: 'API' }, value: '0' }, { text: { type: 'plain_text', text: 'Worker' }, value: '1' }
    ] })
    expect(block(h.posts[0]!, 'c0')!.hint).toBeUndefined()
    await h.submit({
      q0: { selected: { type: 'checkboxes', selected_options: [{ value: '0' }, { value: '1' }] } },
      ...write(0, 'Scheduler')
    })
    expect(h.submissions[0]!.answers).toEqual({ targets: { answers: ['API', 'Worker', 'Scheduler'] } })
  })

  test('accepts the tenth native choice and ids that match object property names', async () => {
    const h = harness({ questions: [{ id: '__proto__', header: 'Target', question: 'Which target?',
      options: Array.from({ length: 10 }, (_, index) => ({ label: `Target ${index}`, description: '' })) }] })
    await h.render()
    expect((await h.submit(pick(0, '9'))).status).toBe(200)
    expect(Object.keys(h.submissions[0]!.answers as JsonObject)).toEqual(['__proto__'])
    expect((h.submissions[0]!.answers as JsonObject).__proto__).toEqual({ answers: ['Target 9'] })
  })

  test('a question that allows no custom answer has no text field and ignores forged text', async () => {
    const h = harness({ questions: [{ id: 'q', header: '', question: 'Proceed?', allowOther: false,
      options: [{ label: 'Yes', description: '' }, { label: 'No', description: '' }] }] })
    await h.render()
    expect(block(h.posts[0]!, 'c0')).toBeUndefined()
    expect(block(h.posts[0]!, 'q0')!.label).toEqual({ type: 'plain_text', text: 'Question 1' })
    await h.submit(write(0, 'Maybe'))
    expect(h.submissions).toHaveLength(0)
    await h.submit({ ...pick(0, '1'), ...write(0, 'Maybe') })
    expect(h.submissions[0]!.answers).toEqual({ q: { answers: ['No'] } })
  })

  test('a button or modal from the earlier paged card answers nothing and shows the form', async () => {
    const h = harness()
    await h.render()
    const legacy = { action_id: 'centaur.user_input:0', value: JSON.stringify({ request: REQUEST, index: 0, choice: 0 }) }
    expect((await h.act(legacy)).status).toBe(200)
    expect(h.submissions).toHaveLength(0)
    expect(buttons(h.updates.at(-1)!).map(button => button.action_id)).toEqual(['centaur.user_input:submit'])
    const before = h.calls.length
    const response = await h.deliver({ type: 'view_submission', team: { id: 'T1' }, user: { id: 'U1' }, view: {
      callback_id: 'centaur.user_input:answer', blocks: [{ type: 'input', block_id: 'custom' }],
      private_metadata: JSON.stringify({ request: REQUEST, index: 0, choice: 'other', channel_id: 'C1' }),
      state: { values: { custom: { text: { value: 'Typed in the old window' } } } }
    } })
    expect(await response.json()).toEqual({ response_action: 'errors', errors: {
      custom: 'Close this window and answer on the question card in the thread.'
    } })
    expect(h.calls).toHaveLength(before)
    expect(h.submissions).toHaveLength(0)
  })

  test('preserves literal question content rather than interpreting Slack mentions', async () => {
    const h = harness({ questions: [{ id: 'q', header: 'Choice', question: 'Use <!channel> or <https://example.com|this>?', options: [] }] })
    await h.render()
    const blocks = h.posts[0]!.blocks as JsonObject[]
    expect(blocks[1]!.text).toEqual({ type: 'plain_text', text: 'Use <!channel> or <https://example.com|this>?' })
    expect(block(h.posts[0]!, 'q0')).toBeUndefined()
    expect(block(h.posts[0]!, 'c0')).toMatchObject({
      label: { type: 'plain_text', text: 'Choice' }, element: { type: 'plain_text_input', multiline: true }
    })
    await h.submit(write(0, 'Use <!channel>'))
    expect(h.submissions[0]!.answers).toEqual({ q: { answers: ['Use <!channel>'] } })
  })

  test('lists choices in full when one exceeds the length Slack allows on a choice', async () => {
    const long = 'Covers every service that reads the ledger, including the reconciliation and export jobs.'
    const h = harness({ questions: [{ id: 'q', header: 'Scope', question: 'Which scope?', options: [
      { label: 'Wide', description: long }, { label: 'Narrow', description: 'One service' }
    ] }] })
    await h.render()
    const blocks = h.posts[0]!.blocks as JsonObject[]
    expect(blocks[1]!.text).toEqual({ type: 'plain_text', text: `Which scope?\n\n1. Wide\n${long}\n\n2. Narrow\nOne service` })
    expect((block(h.posts[0]!, 'q0')!.element as JsonObject).options).toEqual([
      { text: { type: 'plain_text', text: '1. Wide' }, value: '0' }, { text: { type: 'plain_text', text: '2. Narrow' }, value: '1' }
    ])
    await h.submit(pick(0, '0'))
    expect(h.submissions[0]!.answers).toEqual({ q: { answers: ['Wide'] } })
  })

  test('keeps all details and controls within 50 blocks for four maximum-size questions', async () => {
    const questions = Array.from({ length: 4 }, (_, q) => ({
      id: String(q), header: `Question ${q}`, question: 'x'.repeat(2999) + '😀' + 'x'.repeat(2997),
      options: Array.from({ length: 10 }, (_, i) => ({
        label: String(i).repeat(300), description: String.fromCharCode(65 + i).repeat(2000)
      }))
    }))
    const h = harness({ questions })
    await h.render()
    await h.submit({})
    for (const message of [h.posts[0]!, h.updates.at(-1)!]) {
      const blocks = message.blocks as JsonObject[]
      expect(blocks.length).toBeLessThanOrEqual(50)
      expect(buttons(message)).toHaveLength(1)
      const sections = blocks.slice(1).filter(b => b.type === 'section').map(b => (b.text as JsonObject).text as string)
      for (const section of sections) {
        expect([...section].length).toBeLessThanOrEqual(3000)
        expect(section).not.toMatch(/^[\uDC00-\uDFFF]|[\uD800-\uDBFF]$/)
      }
      const expected = questions.map(q => [q.question, ...q.options.map((o, i) =>
        `${i + 1}. ${o.label}\n${o.description}`)].join('\n\n')).join('')
      expect(sections.join('')).toBe(expected)
      expect(blocks.filter(b => b.type === 'input')).toHaveLength(8)
    }
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

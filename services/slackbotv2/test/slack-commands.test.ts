import { describe, expect, spyOn, test } from 'bun:test'
import { createHmac } from 'node:crypto'
import { createMemoryState } from '@chat-adapter/state-memory'
import { Hono } from 'hono'
import type { StateAdapter } from 'chat'
import { mountSlashCommands } from '../src/slack-commands'
import {
  COMMAND_FORM,
  COMMAND_PICKER,
  COMMAND_RETRY,
  COMMAND_SEARCH,
  CommandFieldError,
  type SlackCommandDefinition
} from '../src/slack-command-registry'
import type { SlackbotV2Options } from '../src/types'

const SECRET = 'test-command-signing-secret'
type Recorded = { url: string; body: any }
function signed(body: string) {
  const timestamp = String(Math.floor(Date.now() / 1000))
  const signature =
    'v0=' +
    createHmac('sha256', SECRET).update(`v0:${timestamp}:${body}`).digest('hex')
  return {
    'content-type': 'application/x-www-form-urlencoded',
    'x-slack-request-timestamp': timestamp,
    'x-slack-signature': signature
  }
}
function harness(
  definitions = sampleCommands(true),
  state: StateAdapter = createMemoryState(),
  beforeMount?: (app: Hono) => void
) {
  const calls: Recorded[] = []
  const failures = { open: false, handoff: false }
  let onHandoff: (() => Promise<void>) | undefined
  const options: SlackbotV2Options = {
    signingSecret: SECRET,
    apiKey: 'test-service-key',
    botToken: 'test-bot-token',
    apiUrl: 'http://api.test',
    slackApiUrl: 'http://slack.test/api/',
    slashCommands: { name: '/fineas', teamId: 'T1', definitions },
    fetch: async (url, init) => {
      const call = { url: String(url), body: JSON.parse(String(init?.body)) }
      calls.push(call)
      if (call.url.endsWith('/views.open'))
        return Response.json({ ok: !failures.open })
      if (onHandoff) await onHandoff()
      if (failures.handoff) throw new Error('response lost')
      return Response.json({ ok: true })
    }
  }
  const app = new Hono()
  beforeMount?.(app)
  mountSlashCommands(app, options, state)
  const request = async (path: string, body: string, verified = true) =>
    app.request(path, {
      method: 'POST',
      body,
      headers: verified ? signed(body) : {}
    })
  const slash = (text: string, extra: Record<string, string> = {}) => {
    const body = new URLSearchParams({
      command: '/fineas',
      text,
      trigger_id: 'trigger-1',
      team_id: 'T1',
      channel_id: 'C1',
      user_id: 'U1',
      ...extra
    }).toString()
    return request('/api/slack/commands', body)
  }
  const payload = (value: any, path = '/api/slack/actions', verified = true) =>
    request(
      path,
      new URLSearchParams({ payload: JSON.stringify(value) }).toString(),
      verified
    )
  const open = async (text: string) => {
    expect((await slash(text)).status).toBe(200)
    return calls.findLast((c) => c.url.endsWith('/views.open'))!.body.view
  }
  return {
    app,
    calls,
    failures,
    state,
    request,
    slash,
    payload,
    open,
    setHandoff(fn: () => Promise<void>) {
      onHandoff = fn
    },
    queued() {
      return calls.filter((c) => c.url.endsWith('/api/workflows/runs'))
    }
  }
}

function sampleCommands(enabled: boolean): SlackCommandDefinition[] {
  const create = (kind: 'incident' | 'chore'): SlackCommandDefinition => ({
    name: kind,
    title: kind === 'incident' ? 'Create incident' : 'Create chore',
    description: `Create a ${kind} with an owner and deadline.`,
    keywords: ['create', kind],
    enabled,
    workflowName: 'sample_command',
    example: `${kind} "Investigate" owner:@alex deadline:30m`,
    submitLabel: 'Create',
    fields: [
      { id: 'title', label: 'Description', type: 'text' },
      { id: 'owner', label: 'Owner', type: 'user' },
      { id: 'deadline', label: 'Deadline', type: 'text' }
    ],
    parseText: () => ({ kind }),
    parseForm(values) {
      if (!/^[UW][A-Z0-9]+$/.test(values.owner ?? ''))
        throw new CommandFieldError('owner', 'Choose a Slack workspace member.')
      if (!/^[1-9][0-9]*[mhd]$/.test(values.deadline ?? ''))
        throw new CommandFieldError('deadline', 'Use a duration.')
      return { kind, ...values }
    }
  })
  const actions = [
    'acknowledge',
    'resolve',
    'snooze',
    'reschedule',
    'status'
  ] as const
  return [
    create('incident'),
    create('chore'),
    ...actions.map(
      (name): SlackCommandDefinition => ({
        name,
        title: `${name[0]!.toUpperCase()}${name.slice(1)} request`,
        description: `Apply the ${name} operation to an existing request.`,
        keywords: [name],
        enabled: true,
        workflowName: 'sample_command',
        example: `${name} REQ-42`,
        submitLabel: 'Submit',
        fields: [
          { id: 'key', label: 'Request ID', type: 'text' },
          ...(name === 'snooze' || name === 'reschedule'
            ? [{ id: 'duration', label: 'Duration', type: 'text' as const }]
            : [])
        ],
        parseText: () => ({ operation: name }),
        parseForm(values) {
          const result: Record<string, unknown> = {
            operation: name,
            key: values.key
          }
          if (name === 'snooze' || name === 'reschedule') {
            const duration = /^(\d+)([mhd])$/.exec(values.duration ?? '')
            const scale = { m: 60, h: 3600, d: 86400 }
            const seconds = duration
              ? Number(duration[1]) * scale[duration[2] as keyof typeof scale]
              : 0
            if (!seconds || (name === 'snooze' && seconds > 86400))
              throw new CommandFieldError('duration', 'Use a valid duration.')
            result.duration_seconds = seconds
          }
          return result
        }
      })
    )
  ]
}
function submission(
  view: any,
  values: Record<string, any> = {},
  extra: Record<string, any> = {}
) {
  return {
    type: 'view_submission',
    team: { id: 'T1' },
    user: { id: 'U1' },
    view: { ...view, id: 'V1', state: { values } },
    ...extra
  }
}
function fields(overrides: Record<string, any> = {}) {
  return {
    title: { value: { value: 'Investigate a failing job' } },
    owner: { value: { selected_user: 'U2' } },
    deadline: { value: { value: '30m' } },
    ...overrides
  }
}
function custom(name: string, enabled = true): SlackCommandDefinition {
  return {
    name,
    title: `Run ${name}`,
    description: `Run ${name} task`,
    enabled,
    keywords: ['operations'],
    workflowName: `workflow_${name}`,
    example: `${name} example`,
    submitLabel: 'Run',
    fields: [{ id: 'detail', type: 'text', label: 'Details' }],
    parseText: (text) => ({ detail: text.split(' ').slice(1).join(' ') }),
    parseForm: (values) => ({
      detail: values.detail,
      actor_id: 'spoofed',
      team_id: 'T9',
      channel_id: 'C9'
    })
  }
}

describe('Slack command picker and forms', () => {
  test('overlay routes can handle commands before the workflow registry', async () => {
    const h = harness(sampleCommands(true), createMemoryState(), (app) =>
      app.post('/api/slack/commands', async (c, next) => {
        const form = new URLSearchParams(await c.req.raw.clone().text())
        if (form.get('text') !== 'help') return next()
        return c.json({ response_type: 'ephemeral', text: 'Overlay help' })
      })
    )
    const handled = await (await h.slash('help')).json()
    expect(handled).toEqual({
      response_type: 'ephemeral',
      text: 'Overlay help'
    })
    const unknown = await (await h.slash('missing')).json()
    expect(unknown.text).toContain('Unknown command')
  })

  test('empty command opens searchable picker and bare command opens required form', async () => {
    const h = harness()
    const picker = await h.open('')
    expect(picker.callback_id).toBe(COMMAND_PICKER)
    expect(picker.blocks[1].element.type).toBe('external_select')
    const form = await h.open('incident')
    expect(form.callback_id).toBe(COMMAND_FORM)
    expect(
      form.blocks
        .filter((b: any) => b.type === 'input')
        .map((b: any) => b.block_id)
    ).toEqual(['title', 'owner', 'deadline'])
    expect(
      form.blocks
        .filter((b: any) => b.type === 'input')
        .every((b: any) => b.optional !== true)
    ).toBe(true)
    expect(h.queued()).toHaveLength(0)
  })
  test('picker selection updates to a form without queueing work', async () => {
    const h = harness(),
      view = await h.open('')
    const result = await (
      await h.payload(
        submission(view, {
          command: { [COMMAND_SEARCH]: { selected_option: { value: 'chore' } } }
        })
      )
    ).json()
    expect(result.response_action).toBe('update')
    expect(result.view.title.text).toBe('Create chore')
    expect(result.view.private_metadata).not.toBe(view.private_metadata)
    expect(h.queued()).toHaveLength(0)
    await h.payload(submission(result.view, fields()))
    expect(h.queued()[0]!.body.input.kind).toBe('chore')
  })
  test('external search supports a catalog larger than 100 and excludes disabled commands', async () => {
    const definitions = Array.from({ length: 130 }, (_, i) =>
      custom(`task-${i}`)
    )
    definitions.push(custom('secret-task', false))
    const h = harness(definitions),
      view = await h.open('')
    const search = async (value: string) =>
      (
        await h.payload(
          {
            type: 'block_suggestion',
            action_id: COMMAND_SEARCH,
            team: { id: 'T1' },
            user: { id: 'U1' },
            view,
            value
          },
          '/api/slack/options'
        )
      ).json()
    expect((await search('')).options).toHaveLength(100)
    expect((await search('task-129')).options.map((o: any) => o.value)).toEqual(
      ['task-129']
    )
    expect((await search('secret')).options).toHaveLength(0)
    const unavailable = await (
      await h.payload(
        submission(view, {
          command: {
            [COMMAND_SEARCH]: { selected_option: { value: 'secret-task' } }
          }
        })
      )
    ).json()
    expect(unavailable.response_action).toBe('errors')
    expect(unavailable.errors.command).toBeTruthy()
    expect(h.queued()).toHaveLength(0)
  })
  test('custom domains route to their registered workflow and verified identity', async () => {
    const h = harness([custom('deploy')]),
      view = await h.open('deploy')
    await h.payload(
      submission(
        view,
        { detail: { value: { value: 'staging' } } },
        { channel: { id: 'C9' } }
      )
    )
    expect(h.queued()[0]!.body.workflow_name).toBe('workflow_deploy')
    expect(h.queued()[0]!.body.input).toMatchObject({
      detail: 'staging',
      actor_id: 'U1',
      team_id: 'T1',
      channel_id: 'C1'
    })
    await h.slash('deploy production', { trigger_id: 'trigger-2' })
    expect(h.queued()[1]!.body.workflow_name).toBe('workflow_deploy')
    expect(h.queued()[1]!.body.input.detail).toBe('production')
  })
  test.each(['title', 'owner', 'deadline'])(
    'missing %s returns an inline error and never queues',
    async (field) => {
      const h = harness(),
        view = await h.open('incident')
      const values = fields()
      delete values[field as keyof typeof values]
      const result = await (await h.payload(submission(view, values))).json()
      expect(result.response_action).toBe('errors')
      expect(result.errors[field]).toContain('required')
      expect(h.queued()).toHaveLength(0)
    }
  )
  test('invalid deadline and owner remain field-specific errors', async () => {
    const h = harness(),
      view = await h.open('incident')
    for (const [field, value] of [
      ['deadline', { value: 'yesterday' }],
      ['owner', { selected_user: 'everyone' }]
    ] as const) {
      const result = await (
        await h.payload(submission(view, fields({ [field]: { value } })))
      ).json()
      expect(result.response_action).toBe('errors')
      expect(result.errors[field]).toBeTruthy()
    }
    expect(h.queued()).toHaveLength(0)
  })
  test('opening failures are private errors and never queue work', async () => {
    const h = harness()
    h.failures.open = true
    const response = await (await h.slash('')).json()
    expect(response.response_type).toBe('ephemeral')
    expect(response.text).toContain('Could not open')
    expect(h.queued()).toHaveLength(0)
  })
  test('creation disabled removes create forms but retains status forms', async () => {
    const h = harness(sampleCommands(false))
    expect((await (await h.slash('incident')).json()).text).toContain(
      'disabled'
    )
    expect(h.calls).toHaveLength(0)
    const view = await h.open('status')
    await h.payload(submission(view, { key: { value: { value: 'INC-42' } } }))
    expect(h.queued()[0]!.body.input).toMatchObject({
      operation: 'status',
      key: 'INC-42'
    })
  })
})

describe('Slack command authentication and state recovery', () => {
  test('signed view cannot move to another user or team', async () => {
    const h = harness(),
      view = await h.open('incident')
    for (const extra of [{ user: { id: 'U9' } }, { team: { id: 'T9' } }]) {
      const response = await (
        await h.payload(submission(view, fields(), extra))
      ).json()
      expect(response.view.title.text).toBe('Form unavailable')
    }
    expect(h.queued()).toHaveLength(0)
  })
  test('tampered origin metadata cannot change its channel', async () => {
    const h = harness(),
      view = await h.open('incident')
    const [encoded, signature] = view.private_metadata.split('.')
    const origin = JSON.parse(Buffer.from(encoded, 'base64url').toString())
    origin.channel = 'C9'
    const changed = {
      ...view,
      private_metadata: `${Buffer.from(JSON.stringify(origin)).toString('base64url')}.${signature}`
    }
    const response = await (
      await h.payload(submission(changed, fields()))
    ).json()
    expect(response.view.title.text).toBe('Form unavailable')
    expect(h.queued()).toHaveLength(0)
  })
  test('expired forms are rejected even with a fresh Slack signature', async () => {
    const h = harness(),
      view = await h.open('incident')
    const future = Date.now() + 61 * 60_000
    const clock = spyOn(Date, 'now').mockReturnValue(future)
    try {
      const response = await (
        await h.payload(submission(view, fields()))
      ).json()
      expect(response.view.title.text).toBe('Form unavailable')
      expect(h.queued()).toHaveLength(0)
    } finally {
      clock.mockRestore()
    }
  })
  test('modal and option callbacks require request signatures and bound origin', async () => {
    const h = harness(),
      picker = await h.open('')
    const options = {
      type: 'block_suggestion',
      action_id: COMMAND_SEARCH,
      team: { id: 'T1' },
      user: { id: 'U1' },
      view: picker,
      value: ''
    }
    expect((await h.payload(options, '/api/slack/options', false)).status).toBe(
      401
    )
    expect(
      (
        await h.payload(
          { ...options, user: { id: 'U9' } },
          '/api/slack/options'
        )
      ).status
    ).toBe(403)
    expect(
      (await h.payload(submission(picker), '/api/slack/actions', false)).status
    ).toBe(401)
    expect(h.queued()).toHaveLength(0)
  })
  test('lost handoff retries saved input across router restart and duplicate submission', async () => {
    const h = harness(),
      view = await h.open('incident')
    h.failures.handoff = true
    const first = await (await h.payload(submission(view, fields()))).json()
    expect(first.view.callback_id).toBe(COMMAND_RETRY)
    const original = structuredClone(h.queued()[0]!.body)
    const restarted = harness(sampleCommands(true), h.state)
    const response = await (
      await restarted.payload(
        submission(
          first.view,
          fields({ title: { value: { value: 'changed after timeout' } } })
        )
      )
    ).json()
    expect(response.view.title.text).toBe('Request received')
    expect(restarted.queued()[0]!.body).toEqual(original)
    await restarted.payload(submission(view, fields()))
    expect(restarted.queued()).toHaveLength(1)
  })
  test('concurrent submits cause one handoff and replay the receipt', async () => {
    const h = harness(),
      view = await h.open('incident')
    let signalStarted!: () => void, signalFinish!: () => void
    const started = new Promise<void>((resolve) => {
      signalStarted = resolve
    })
    const finish = new Promise<void>((resolve) => {
      signalFinish = resolve
    })
    h.setHandoff(async () => {
      signalStarted()
      await finish
    })
    const first = h.payload(submission(view, fields()))
    await started
    const second = await (await h.payload(submission(view, fields()))).json()
    expect(second.view.callback_id).toBe(COMMAND_RETRY)
    signalFinish()
    expect((await (await first).json()).view.title.text).toBe(
      'Request received'
    )
    await h.payload(submission(second.view))
    expect(h.queued()).toHaveLength(1)
  })
  test('unrelated and malformed action bodies survive fallback', async () => {
    for (const value of [
      null,
      { view: { callback_id: 5 } },
      { actions: [{ action_id: 'other' }] }
    ]) {
      const h = harness()
      h.app.post('/api/slack/actions', async (c) => c.text(await c.req.text()))
      const body = new URLSearchParams({
        payload: JSON.stringify(value)
      }).toString()
      expect(
        await (await h.request('/api/slack/actions', body, false)).text()
      ).toBe(body)
      expect(h.queued()).toHaveLength(0)
    }
    const h = harness()
    h.app.post('/api/slack/actions', async (c) => c.text(await c.req.text()))
    expect(
      await (
        await h.request('/api/slack/actions', 'payload=not-json', false)
      ).text()
    ).toBe('payload=not-json')
  })
  test('text retries preserve the original accepted deadline', async () => {
    let parses = 0
    const command = custom('fixed')
    command.parseText = () => ({
      created_at: `parse-${++parses}`,
      deadline_at: `deadline-${parses}`
    })
    const h = harness([command])
    h.failures.handoff = true
    expect((await h.slash('fixed detail')).status).toBe(503)
    const original = structuredClone(h.queued()[0]!.body)
    h.failures.handoff = false
    expect((await h.slash('fixed detail')).status).toBe(200)
    expect(h.queued()[1]!.body).toEqual(original)
  })
  test('accepted text replay does not revalidate already frozen input', async () => {
    let parses = 0
    const command = custom('once')
    command.parseText = () => {
      if (++parses > 1) throw new Error('original deadline has passed')
      return { detail: 'accepted original input' }
    }
    const h = harness([command])
    expect((await (await h.slash('once detail')).json()).text).toContain(
      'Request received'
    )
    expect((await (await h.slash('once detail')).json()).text).toContain(
      'Request received'
    )
    expect(h.queued()).toHaveLength(1)
  })
  test('different picker choices have independent durable submission identities', async () => {
    const h = harness(),
      picker = await h.open('')
    for (const name of ['incident', 'chore']) {
      const form = await (
        await h.payload(
          submission(picker, {
            command: { [COMMAND_SEARCH]: { selected_option: { value: name } } }
          })
        )
      ).json()
      await h.payload(submission(form.view, fields()))
    }
    expect(h.queued()).toHaveLength(2)
    expect(h.queued()[0]!.body.idempotency_key).not.toBe(
      h.queued()[1]!.body.idempotency_key
    )
    expect(h.queued().map((c) => c.body.input.kind)).toEqual([
      'incident',
      'chore'
    ])
  })
  test.each(['acknowledge', 'resolve', 'snooze', 'reschedule', 'status'])(
    'bare %s validates its form and dispatches the expected action',
    async (name) => {
      const h = harness(),
        form = await h.open(name)
      const values = {
        key: { value: { value: 'INC-42' } },
        duration: { value: { value: '15m' } }
      }
      const response = await (await h.payload(submission(form, values))).json()
      expect(response.view.title.text).toBe('Request received')
      expect(h.queued()[0]!.body.input.operation).toBe(name)
      expect(h.queued()[0]!.body.input.key).toBe('INC-42')
      if (name === 'snooze' || name === 'reschedule')
        expect(h.queued()[0]!.body.input.duration_seconds).toBe(900)
    }
  )
  test('too-long snooze shows duration error and can be corrected', async () => {
    const h = harness(),
      form = await h.open('snooze')
    const bad = {
      key: { value: { value: 'INC-42' } },
      duration: { value: { value: '2d' } }
    }
    expect(
      (await (await h.payload(submission(form, bad))).json()).errors.duration
    ).toBeTruthy()
    expect(h.queued()).toHaveLength(0)
    bad.duration.value.value = '5m'
    await h.payload(submission(form, bad))
    expect(h.queued()[0]!.body.input.duration_seconds).toBe(300)
  })

  test('stalled submission storage returns before Slack timeout and cannot queue late', async () => {
    const h = harness(),
      view = await h.open('incident')
    const originalGet = h.state.get.bind(h.state)
    let unblock!: () => void
    const stalled = new Promise<void>((resolve) => {
      unblock = resolve
    })
    const get = spyOn(h.state, 'get').mockImplementation(async (key) => {
      await stalled
      return originalGet(key)
    })
    try {
      const started = performance.now()
      const response = await h.payload(submission(view, fields()))
      expect(performance.now() - started).toBeLessThan(2900)
      expect((await response.json()).view.callback_id).toBe(COMMAND_RETRY)
      expect(h.queued()).toHaveLength(0)
      unblock()
      await new Promise((resolve) => setTimeout(resolve, 30))
      expect(h.queued()).toHaveLength(0)
    } finally {
      unblock()
      get.mockRestore()
    }
  })

  test('failure persisting initial input never performs workflow handoff', async () => {
    const h = harness(),
      view = await h.open('incident')
    const write = spyOn(h.state, 'setIfNotExists').mockRejectedValue(
      new Error('database unavailable')
    )
    try {
      const response = await (
        await h.payload(submission(view, fields()))
      ).json()
      expect(response.view.callback_id).toBe(COMMAND_RETRY)
      expect(h.queued()).toHaveLength(0)
    } finally {
      write.mockRestore()
    }
  })
})

test('signed form carries optional multiline details and multiple owners to the workflow', async () => {
  const command = sampleCommands(true)[0]!
  command.fields = [
    ...command.fields,
    {
      id: 'description',
      label: 'Description',
      type: 'text',
      multiline: true,
      optional: true,
      maxLength: 2000
    },
    {
      id: 'additional_owners',
      label: 'Additional owners',
      type: 'users',
      optional: true,
      maxSelectedItems: 9
    }
  ]
  const h = harness([command])
  const view = await h.open('incident')
  const result = await h.payload(
    submission(
      view,
      fields({
        description: { value: { value: 'First line\nSecond line' } },
        additional_owners: { value: { selected_users: ['U3', 'U4'] } }
      })
    )
  )
  expect(result.status).toBe(200)
  expect(h.queued()).toHaveLength(1)
  expect(h.queued()[0]!.body.input).toMatchObject({
    description: 'First line\nSecond line',
    additional_owners: 'U3,U4',
    actor_id: 'U1',
    channel_id: 'C1'
  })
})

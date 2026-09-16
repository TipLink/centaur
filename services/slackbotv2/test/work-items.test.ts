import { describe, expect, test } from 'bun:test'
import { createHmac } from 'node:crypto'
import { Hono } from 'hono'
import type { Message } from 'chat'
import { createMemoryState } from '@chat-adapter/state-memory'
import { mountWorkItems } from '../src/work-items'
import {
  durationSeconds,
  parseWorkItemCommand,
  workItemCommands
} from '../src/work-item-commands'
import { mountSlashCommands, verifySlackSignature } from '../src/slack-commands'
import type { SlackbotV2Options } from '../src/types'

const secret = 'test-signing-secret'
const fixedNow = new Date('2026-09-16T12:00:00Z')
function signature(body: string, timestamp: string) {
  return (
    'v0=' +
    createHmac('sha256', secret).update(`v0:${timestamp}:${body}`).digest('hex')
  )
}
function signed(body: string) {
  const timestamp = String(Math.floor(Date.now() / 1000))
  return {
    'content-type': 'application/x-www-form-urlencoded',
    'x-slack-request-timestamp': timestamp,
    'x-slack-signature': signature(body, timestamp)
  }
}
function harness(
  member = true,
  getThread?: Parameters<typeof mountWorkItems>[3],
  webhookStatus = 200,
  enabled = true
) {
  const calls: Array<{
    url: string
    body: any
    rawBody: string
    headers: Headers
  }> = []
  const state = createMemoryState()
  const app = new Hono()
  const options: SlackbotV2Options = {
    apiUrl: 'http://api.test',
    apiKey: 'test-api-key',
    botToken: 'test-bot',
    signingSecret: secret,
    slackApiUrl: 'http://slack.test/api/',
    slashCommands: {
      name: '/fineas',
      teamId: 'T1',
      definitions: workItemCommands(enabled)
    },
    fetch: async (input, init) => {
      const url = String(input)
      const body = init?.body ? JSON.parse(String(init.body)) : undefined
      calls.push({
        url,
        body,
        rawBody: String(init?.body ?? ''),
        headers: new Headers(init?.headers)
      })
      if (url.endsWith('/api/webhooks/work-items-linear'))
        return new Response(null, { status: webhookStatus })
      if (url.endsWith('/auth.test'))
        return Response.json({ ok: true, team_id: 'T1' })
      if (url.endsWith('/conversations.info'))
        return Response.json({
          ok: true,
          channel: { id: 'C1', name: 'incidents', is_member: member }
        })
      if (url.endsWith('/users.info'))
        return Response.json({
          ok: true,
          user: {
            id: 'U1',
            team_id: 'T1',
            name: 'test-user',
            profile: { email: 'test@example.test' }
          }
        })
      return Response.json({ ok: true })
    }
  }
  mountSlashCommands(app, options, state)
  mountWorkItems(app, options, state, getThread)
  return { app, calls, state }
}
function command(text: string, extra: Record<string, string> = {}) {
  return new URLSearchParams({
    command: '/fineas',
    team_id: 'T1',
    channel_id: 'C1',
    user_id: 'U1',
    trigger_id: 'trigger-1',
    text,
    ...extra
  }).toString()
}

describe('work-item parsing and signatures', () => {
  test('creation and explicit timezone deadlines', () => {
    expect(
      parseWorkItemCommand(
        'incident "Investigate payouts" owner:<@U2|alex> deadline:30m',
        fixedNow
      )
    ).toEqual({
      operation: 'create',
      kind: 'incident',
      title: 'Investigate payouts',
      owner_id: 'U2',
      created_at: fixedNow.toISOString(),
      deadline_at: '2026-09-16T12:30:00.000Z'
    })
    expect(
      parseWorkItemCommand(
        'chore "Rotate key" deadline:2026-09-16T15:00:00+02:00 owner:<@U2>',
        fixedNow
      ).deadline_at
    ).toBe('2026-09-16T13:00:00.000Z')
    expect(parseWorkItemCommand('snooze INC-42 15m')).toEqual({
      operation: 'snooze',
      key: 'INC-42',
      duration_seconds: 900
    })
  })
  test.each([
    'incident "Investigate" deadline:30m',
    'incident "Investigate" owner:<@U2>',
    'incident "Investigate" owner:<@U2> deadline:0m',
    'incident "Investigate" owner:<@U2> deadline:30m unknown:value',
    'incident "Investigate" owner:<@U2> owner:<@U3> deadline:30m',
    'incident "Investigate" owner:<@U2> deadline:2026-09-16T15:00',
    'incident "Investigate" owner:<@U2> deadline:2020-01-01T00:00Z',
    'incident Investigate owner:<@U2> deadline:30m',
    'resolve INC-0',
    'incident "Invalid day" owner:<@U2> deadline:2027-02-30T15:00Z',
    'resolve INC-42 extra',
    'snooze CHR-1',
    'news INC-42'
  ])('rejects invalid command %s', (text) => {
    expect(() => parseWorkItemCommand(text, fixedNow)).toThrow()
  })
  test('duration bounded to one year', () => {
    expect(durationSeconds('366d')).toBe(366 * 86400)
    expect(() => durationSeconds('367d')).toThrow()
    expect(() => durationSeconds('9999999999999999999m')).toThrow()
  })
  test('signatures authenticate exact bytes and freshness', () => {
    const body = 'command=abc',
      timestamp = String(fixedNow.getTime() / 1000)
    expect(
      verifySlackSignature(
        body,
        timestamp,
        signature(body, timestamp),
        secret,
        fixedNow.getTime()
      )
    ).toBe(true)
    expect(
      verifySlackSignature(
        body + 'x',
        timestamp,
        signature(body, timestamp),
        secret,
        fixedNow.getTime()
      )
    ).toBe(false)
    expect(
      verifySlackSignature(
        body,
        timestamp,
        signature(body, timestamp),
        secret,
        fixedNow.getTime() + 301000
      )
    ).toBe(false)
    expect(
      verifySlackSignature(
        body,
        timestamp,
        'v0=bad',
        secret,
        fixedNow.getTime()
      )
    ).toBe(false)
  })
})

describe('work-item ingress and context', () => {
  test('invalid and unauthenticated commands have no workflow side effects', async () => {
    const { app, calls } = harness()
    const body = command('incident "Investigate" deadline:30m')
    const response = await app.request('/api/slack/commands', {
      method: 'POST',
      body,
      headers: signed(body)
    })
    expect(response.status).toBe(200)
    expect((await response.json()).text).toContain('Example: /fineas')
    expect(calls).toHaveLength(0)
    const unauthorized = await app.request('/api/slack/commands', {
      method: 'POST',
      body
    })
    expect(unauthorized.status).toBe(401)
    expect(calls).toHaveLength(0)
  })
  test('valid command handoff uses stable idempotency and verified identity', async () => {
    const { app, calls } = harness()
    const body = command('incident "Investigate" owner:<@U2> deadline:30m')
    for (let i = 0; i < 2; i++)
      expect(
        (
          await app.request('/api/slack/commands', {
            method: 'POST',
            body,
            headers: signed(body)
          })
        ).status
      ).toBe(200)
    expect(calls).toHaveLength(1)
    expect(calls[0]!.body.idempotency_key).toStartWith('slack-command:')
    expect(calls[0]!.body.input.actor_id).toBe('U1')
    expect(calls[0]!.body.input.team_id).toBe('T1')
  })
  test('rejects cross-workspace and direct-message commands', async () => {
    for (const scope of [{ team_id: 'T2' }, { channel_id: 'D1' }] as Array<
      Record<string, string>
    >) {
      const { app, calls } = harness()
      const body = command('resolve INC-1', scope)
      await app.request('/api/slack/commands', {
        method: 'POST',
        body,
        headers: signed(body)
      })
      expect(calls).toHaveLength(0)
    }
  })
  test('button restricts operation and requires verified signature', async () => {
    const { app, calls } = harness()
    const body = new URLSearchParams({
      payload: JSON.stringify({
        team: { id: 'T1' },
        channel: { id: 'C1' },
        user: { id: 'U1' },
        actions: [
          {
            action_id: 'work_item:test',
            value: 'reschedule INC-1 15m',
            action_ts: '123.456'
          }
        ]
      })
    }).toString()
    expect(
      (
        await app.request('/api/slack/actions', {
          method: 'POST',
          body,
          headers: signed(body)
        })
      ).status
    ).toBe(400)
    expect(
      (await app.request('/api/slack/actions', { method: 'POST', body })).status
    ).toBe(401)
    expect(calls).toHaveLength(0)
  })
  test('internal bridge rejects missing auth and bot non-membership', async () => {
    const { app, calls } = harness(false)
    const body = JSON.stringify({
      operation: 'post',
      args: { team_id: 'T1', channel: 'C1', text: 'test' }
    })
    expect(
      (
        await app.request('/internal/workflow/slack', {
          method: 'POST',
          body,
          headers: { 'content-type': 'application/json' }
        })
      ).status
    ).toBe(401)
    expect(calls).toHaveLength(0)
    expect(
      (
        await app.request('/internal/workflow/slack', {
          method: 'POST',
          body,
          headers: {
            'content-type': 'application/json',
            authorization: 'Bearer test-api-key'
          }
        })
      ).status
    ).toBe(403)
    expect(calls.some((c) => c.url.endsWith('/chat.postMessage'))).toBe(false)
  })
  test('context durably appends and subscribes without execution', async () => {
    const { app, calls, state } = harness()
    await state.connect()
    try {
      const body = JSON.stringify({
        operation: 'context',
        args: {
          team_id: 'T1',
          channel: 'C1',
          thread_ts: '1700000000.123456',
          creator_id: 'U1',
          event_id: 'work-item:1',
          text: 'Incident context'
        }
      })
      const response = await app.request('/internal/workflow/slack', {
        method: 'POST',
        body,
        headers: {
          'content-type': 'application/json',
          authorization: 'Bearer test-api-key'
        }
      })
      expect(response.status).toBe(200)
      expect(await state.isSubscribed('slack:C1:1700000000.123456')).toBe(true)
      expect(calls.some((c) => c.url.endsWith('/messages'))).toBe(true)
      expect(calls.some((c) => c.url.endsWith('/execute'))).toBe(false)
    } finally {
      await state.disconnect()
    }
  })
  test('unrelated commands and actions preserve raw body for Chat SDK', async () => {
    for (const path of ['/api/slack/commands', '/api/slack/actions']) {
      const { app } = harness()
      app.post(path, async (c) => c.text(await c.req.text()))
      const body = path.endsWith('commands')
        ? 'command=%2Fother&text=hello'
        : 'payload=' +
          encodeURIComponent(
            JSON.stringify({ actions: [{ action_id: 'other' }] })
          )
      const response = await app.request(path, {
        method: 'POST',
        body,
        headers: signed(body)
      })
      expect(response.status).toBe(200)
      expect(await response.text()).toBe(body)
    }
  })

  test('context subscribes before catching up and history initializes only once', async () => {
    let historyReads = 0
    const threadId = 'slack:C1:1700000000.654321'
    const { app, calls, state } = harness(true, (id) => ({
      allMessages: (async function* () {
        historyReads++
        expect(id).toBe(threadId)
        expect(await state.isSubscribed(threadId)).toBe(true)
        yield {
          id: '1700000001.000001',
          threadId,
          text: 'Investigating the timeout',
          raw: { team: 'T1', channel: 'C1' },
          author: {
            userId: 'U2',
            userName: 'owner',
            fullName: 'Owner',
            isBot: false,
            isMe: false
          },
          attachments: [],
          links: [],
          metadata: { dateSent: new Date('2026-09-16T12:00:00Z') }
        } as unknown as Message
      })()
    }))
    await state.connect()
    try {
      for (const eventId of ['seed-1', 'update-2']) {
        const body = JSON.stringify({
          operation: 'context',
          args: {
            team_id: 'T1',
            channel: 'C1',
            thread_ts: '1700000000.654321',
            creator_id: 'U1',
            event_id: eventId,
            text: 'Current incident context'
          }
        })
        expect(
          (
            await app.request('/internal/workflow/slack', {
              method: 'POST',
              body,
              headers: {
                'content-type': 'application/json',
                authorization: 'Bearer test-api-key'
              }
            })
          ).status
        ).toBe(200)
      }
      expect(historyReads).toBe(1)
      const appends = calls.filter((c) => c.url.endsWith('/messages'))
      expect(appends).toHaveLength(2)
      expect(JSON.stringify(appends[0]!.body)).toContain(
        'Investigating the timeout'
      )
      expect(JSON.stringify(appends[1]!.body)).not.toContain(
        'Investigating the timeout'
      )
      expect(calls.some((c) => c.url.endsWith('/execute'))).toBe(false)
    } finally {
      await state.disconnect()
    }
  })
  test('malformed internal JSON and button identities are rejected', async () => {
    const { app, calls } = harness()
    expect(
      (
        await app.request('/internal/workflow/slack', {
          method: 'POST',
          body: 'bad json',
          headers: { authorization: 'Bearer test-api-key' }
        })
      ).status
    ).toBe(400)
    const body = new URLSearchParams({
      payload: JSON.stringify({
        team: { id: 'T1' },
        channel: { id: 'C1' },
        actions: [
          {
            action_id: 'work_item:resolve',
            value: 'resolve INC-1',
            action_ts: '123.456'
          }
        ]
      })
    }).toString()
    expect(
      (
        await app.request('/api/slack/actions', {
          method: 'POST',
          body,
          headers: signed(body)
        })
      ).status
    ).toBe(400)
    expect(calls).toHaveLength(0)
  })

  test('null and numeric action payloads fall through without consuming the body', async () => {
    for (const payload of [null, { actions: [{ action_id: 123 }] }]) {
      const { app, calls } = harness()
      app.post('/api/slack/actions', async (c) => c.text(await c.req.text()))
      const body = new URLSearchParams({
        payload: JSON.stringify(payload)
      }).toString()
      const response = await app.request('/api/slack/actions', {
        method: 'POST',
        body
      })
      expect(response.status).toBe(200)
      expect(await response.text()).toBe(body)
      expect(calls).toHaveLength(0)
    }
  })

  test('Linear webhook forwards exact bytes and signatures and maps acceptance to 200', async () => {
    for (const status of [202, 401, 503]) {
      const { app, calls } = harness(true, undefined, status)
      const body = '{ "type": "Issue", "action": "update" }'
      const response = await app.request('/api/webhooks/work-items-linear', {
        method: 'POST',
        body,
        headers: {
          'content-type': 'application/json',
          'linear-signature': 'signed-test-body',
          'linear-delivery': 'delivery-1'
        }
      })
      expect(response.status).toBe(status === 202 ? 200 : status)
      expect(calls[0]!.rawBody).toBe(body)
      expect(calls[0]!.headers.get('linear-signature')).toBe('signed-test-body')
      expect(calls[0]!.headers.get('linear-delivery')).toBe('delivery-1')
    }
  })
  test('disabled intake rejects creation but leaves existing actions available', async () => {
    const { app, calls } = harness(true, undefined, 200, false)
    const create = command('chore "Rotate key" owner:<@U2> deadline:1h')
    const response = await app.request('/api/slack/commands', {
      method: 'POST',
      body: create,
      headers: signed(create)
    })
    expect((await response.json()).text).toContain('disabled')
    expect(calls).toHaveLength(0)
    const resolve = command('resolve INC-1')
    expect(
      (
        await app.request('/api/slack/commands', {
          method: 'POST',
          body: resolve,
          headers: signed(resolve)
        })
      ).status
    ).toBe(200)
    expect(calls[0]!.body.input.operation).toBe('resolve')
  })
})

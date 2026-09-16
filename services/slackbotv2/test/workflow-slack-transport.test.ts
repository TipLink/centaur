import { describe, expect, test } from 'bun:test'
import { Hono } from 'hono'
import type { Message } from 'chat'
import { createMemoryState } from '@chat-adapter/state-memory'
import { mountWorkflowSlackTransport } from '../src/workflow-slack-transport'
import type { SlackbotV2Options } from '../src/types'

function harness(
  member = true,
  getThread?: Parameters<typeof mountWorkflowSlackTransport>[3]
) {
  const calls: Array<{ url: string; body: any }> = []
  const state = createMemoryState()
  const app = new Hono()
  const options: SlackbotV2Options = {
    apiUrl: 'http://api.test',
    apiKey: 'test-api-key',
    botToken: 'test-bot',
    signingSecret: 'test-signing-secret',
    slashCommands: { name: '/example', teamId: 'T1', definitions: [] },
    slackApiUrl: 'http://slack.test/api/',
    fetch: async (input, init) => {
      const url = String(input)
      const body = init?.body ? JSON.parse(String(init.body)) : undefined
      calls.push({ url, body })
      if (url.endsWith('/auth.test'))
        return Response.json({ ok: true, team_id: 'T1' })
      if (url.endsWith('/conversations.info'))
        return Response.json({
          ok: true,
          channel: { id: 'C1', is_member: member }
        })
      return Response.json({ ok: true })
    }
  }
  mountWorkflowSlackTransport(app, options, state, getThread)
  return { app, calls, state }
}

const request = (operation: string, args: Record<string, unknown>) => ({
  method: 'POST',
  body: JSON.stringify({ operation, args }),
  headers: {
    'content-type': 'application/json',
    authorization: 'Bearer test-api-key'
  }
})

describe('workflow Slack transport', () => {
  test('requires service authentication and channel membership', async () => {
    const { app, calls } = harness(false)
    const body = JSON.stringify({
      operation: 'post',
      args: { team_id: 'T1', channel: 'C1', text: 'test' }
    })
    expect(
      (await app.request('/internal/workflow/slack', { method: 'POST', body }))
        .status
    ).toBe(401)
    expect(
      (await app.request('/internal/workflow/slack', request('post', {
        team_id: 'T1', channel: 'C1', text: 'test'
      }))).status
    ).toBe(403)
    expect(calls.some((call) => call.url.endsWith('/chat.postMessage'))).toBe(
      false
    )
  })

  test('rejects another workspace and unsupported operations', async () => {
    const { app } = harness()
    expect(
      (await app.request('/internal/workflow/slack', request('post', {
        team_id: 'T2', channel: 'C1', text: 'test'
      }))).status
    ).toBe(403)
    expect(
      (await app.request('/internal/workflow/slack', request('delete', {
        team_id: 'T1', channel: 'C1'
      }))).status
    ).toBe(400)
  })

  test('context subscribes before catch-up and initializes history once', async () => {
    let historyReads = 0
    const threadId = 'slack:C1:1700000000.654321'
    let stateRef: ReturnType<typeof createMemoryState>
    const h = harness(true, (id) => ({
      allMessages: (async function* () {
        historyReads++
        expect(id).toBe(threadId)
        expect(await stateRef.isSubscribed(threadId)).toBe(true)
        yield {
          id: '1700000001.000001',
          threadId,
          text: 'Existing context',
          raw: { team: 'T1', channel: 'C1' },
          author: {
            userId: 'U2', userName: 'owner', fullName: 'Owner',
            isBot: false, isMe: false
          },
          attachments: [],
          links: [],
          metadata: { dateSent: new Date('2026-09-16T12:00:00Z') }
        } as unknown as Message
      })()
    }))
    stateRef = h.state
    await h.state.connect()
    try {
      for (const eventId of ['seed-1', 'update-2']) {
        const response = await h.app.request(
          '/internal/workflow/slack',
          request('context', {
            team_id: 'T1',
            channel: 'C1',
            thread_ts: '1700000000.654321',
            creator_id: 'U1',
            event_id: eventId,
            text: 'Current context'
          })
        )
        expect(response.status).toBe(200)
      }
      expect(historyReads).toBe(1)
      const appends = h.calls.filter((call) => call.url.endsWith('/messages'))
      expect(appends).toHaveLength(2)
      expect(JSON.stringify(appends[0]!.body)).toContain('Existing context')
      expect(JSON.stringify(appends[1]!.body)).not.toContain('Existing context')
      expect(h.calls.some((call) => call.url.endsWith('/execute'))).toBe(false)
    } finally {
      await h.state.disconnect()
    }
  })

  test('forwards only the bounded operation map', async () => {
    const { app, calls } = harness()
    const response = await app.request(
      '/internal/workflow/slack',
      request('post', { team_id: 'T1', channel: 'C1', text: 'hello' })
    )
    expect(response.status).toBe(200)
    expect(calls.some((call) => call.url.endsWith('/chat.postMessage'))).toBe(
      true
    )
  })
})

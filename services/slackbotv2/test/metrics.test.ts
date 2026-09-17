import { describe, expect, test } from 'bun:test'
import { createMemoryState } from '@chat-adapter/state-memory'
import { createSlackbotV2 } from '../src/index'
import { resetSlackbotMetricsForTests, slackbotMetrics } from '../src/metrics'

describe('slackbotv2 metrics', () => {
  test('withholds health until state connects', async () => {
    const state = createMemoryState()
    const originalConnect = state.connect.bind(state)
    let connectCalls = 0
    let releaseConnect!: () => void
    const connectGate = new Promise<void>(resolve => {
      releaseConnect = resolve
    })
    state.connect = async () => {
      connectCalls += 1
      await connectGate
      await originalConnect()
    }
    const bot = createSlackbotV2({
      apiUrl: 'http://api.test',
      botToken: 'xoxb-test',
      recoverRenderObligationsOnStart: false,
      signingSecret: 'secret',
      state
    })

    const notReadyResponse = await bot.app.request('/health')
    expect(notReadyResponse.status).toBe(503)
    await expect(notReadyResponse.json()).resolves.toMatchObject({
      ok: false,
      service: 'slackbotv2',
      database_connected: false,
      database_status: 'connecting'
    })
    const liveResponse = await bot.app.request('/live')
    expect(liveResponse.status).toBe(200)
    await expect(liveResponse.json()).resolves.toEqual({
      ok: true,
      service: 'slackbotv2'
    })

    releaseConnect()
    const readyResponse = await waitForHealthy(bot)
    expect(readyResponse.status).toBe(200)
    await expect(readyResponse.json()).resolves.toMatchObject({
      ok: true,
      service: 'slackbotv2',
      database_connected: true
    })
    expect(connectCalls).toBe(1)
  })

  test('serves Prometheus text metrics', async () => {
    resetSlackbotMetricsForTests()
    slackbotMetrics.webhookRequests.inc({
      event_type: 'app_mention',
      outcome: 'success',
      route: '/api/webhooks/slack'
    })
    slackbotMetrics.sessionDelivery.inc({
      delivery_status: 'streamed'
    })

    const bot = createSlackbotV2({
      apiUrl: 'http://api.test',
      botToken: 'xoxb-test',
      recoverRenderObligationsOnStart: false,
      signingSecret: 'secret',
      state: createMemoryState()
    })

    const response = await bot.app.request('/metrics')
    const body = await response.text()

    expect(response.status).toBe(200)
    expect(response.headers.get('content-type')).toContain('text/plain')
    expect(body).toContain('# HELP slackbotv2_info Static Slackbot v2 service info.')
    expect(body).toContain('slackbotv2_info 1')
    expect(body).toContain(
      'slackbotv2_slack_webhook_requests_total{route="/api/webhooks/slack",event_type="app_mention",outcome="success"} 1'
    )
    expect(body).toContain(
      'centaur_session_delivery_total{delivery_status="streamed"} 1'
    )
  })

  test('keeps Socket Mode unready until its listener initializes', async () => {
    const bot = createSlackbotV2({
      apiUrl: 'http://api.test',
      appToken: 'xapp-test',
      botToken: 'xoxb-test',
      recoverRenderObligationsOnStart: false,
      slackMode: 'socket',
      state: createMemoryState()
    })

    const response = await waitForDatabaseConnected(bot)
    expect(response.status).toBe(503)
    await expect(response.json()).resolves.toMatchObject({
      ok: false,
      database_connected: true,
      database_status: 'connected',
      slack_transport_status: 'connecting'
    })
  })
})

async function waitForHealthy(bot: ReturnType<typeof createSlackbotV2>): Promise<Response> {
  for (let attempt = 0; attempt < 20; attempt++) {
    const response = await bot.app.request('/health')
    if (response.status === 200) return response
    await sleep(5)
  }
  return bot.app.request('/health')
}

async function waitForDatabaseConnected(
  bot: ReturnType<typeof createSlackbotV2>
): Promise<Response> {
  for (let attempt = 0; attempt < 20; attempt++) {
    const response = await bot.app.request('/health')
    const body = await response.clone().json() as { database_connected?: boolean }
    if (body.database_connected) return response
    await sleep(5)
  }
  return bot.app.request('/health')
}

function sleep(ms: number): Promise<void> {
  return new Promise(resolve => setTimeout(resolve, ms))
}

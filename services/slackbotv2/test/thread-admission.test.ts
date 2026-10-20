import { expect, test } from 'bun:test'
import { admitSlackThread } from '../src/thread-admission'
import type { SlackbotV2Options } from '../src/types'

const body = JSON.stringify({ type: 'event_callback', team_id: 'THOME', event: {
  channel: 'CPROTECTED', type: 'app_mention', user: 'UOWNER', ts: '1792186000.000001'
} })
const request = new Request('http://fixture', { headers: { 'x-slack-signature': 'v0=fixture', 'x-slack-request-timestamp': '1792186000' } })
const options: SlackbotV2Options = { apiUrl: 'http://api', botToken: 'fixture', signingSecret: 'fixture',
  channelUserPolicy: { CPROTECTED: { team_id: 'THOME', allowed_user_ids: ['UOWNER'] } },
  threadAdmissionUrl: 'http://ownership/slack/thread-admission' }

test('passes original signature and body to owner gate; respects deny without a fallback', async () => {
  for (const allowed of [true, false]) {
    const fetchFn = (async (url: RequestInfo | URL, init?: RequestInit) => {
      expect(String(url)).toBe(String(options.threadAdmissionUrl))
      expect(init?.body).toBe(body)
      expect(new Headers(init?.headers).get('x-slack-signature')).toBe('v0=fixture')
      return Response.json({ allowed })
    }) as unknown as NonNullable<SlackbotV2Options["fetch"]>
    expect(await admitSlackThread(request, body, { ...options, fetch: fetchFn })).toBe(allowed)
  }
})
test('errors and malformed responses fail closed; unrelated channels stay independent', async () => {
  for (const response of [new Response('unavailable', { status: 503 }), Response.json({}), Response.json({ allowed: 'true' })]) {
    const fetchFn = (async () => response) as unknown as NonNullable<SlackbotV2Options["fetch"]>
    expect(admitSlackThread(request, body, { ...options, fetch: fetchFn })).rejects.toThrow()
  }
  expect(await admitSlackThread(request, body.replace('CPROTECTED', 'COTHER'), { ...options,
    fetch: (async () => { throw new Error('must not call') }) as unknown as NonNullable<SlackbotV2Options["fetch"]> })).toBe(true)
})

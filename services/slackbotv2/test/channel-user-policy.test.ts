import { describe, expect, it } from 'bun:test'
import { channelWebhookAllowed, parseChannelUserPolicy } from '../src/channel-user-policy'
import { isAllowedSlackWebhookBody, isAllowedSlackMessage } from '../src/slack-events'
import type { Logger, Message } from 'chat'

const policy = parseChannelUserPolicy(JSON.stringify({ CPROTECTED: { team_id: 'THOME', allowed_user_ids: ['UAPPROVED'] } }))
const logger: Logger = { debug() {}, info() {}, warn() {}, error() {}, child: () => logger }
const options = { apiUrl: 'http://localhost', botToken: 'test', signingSecret: 'test', channelUserPolicy: policy }
const event = (user: string | undefined, channel = 'CPROTECTED', type = 'app_mention') => ({
  type: 'event_callback', team_id: 'THOME', event: { type, user, channel }
})

describe('channel user admission', () => {
  it('requires a nonempty valid allowlist and fails startup on malformed policy', () => {
    expect(parseChannelUserPolicy(undefined)).toEqual({})
    for (const raw of ['', '[]', 'null', '{', '{"CPROTECTED":{"team_id":"THOME","allowed_user_ids":[]}}',
      '{"CPROTECTED":{"team_id":"THOME","allowed_user_ids":["*"]}}',
      '{"CPROTECTED":{"team_id":"THOME","allowed_user_ids":["UONE","UONE"]}}']) {
      expect(() => parseChannelUserPolicy(raw)).toThrow()
    }
  })
  it('allows approved humans, denies others and leaves other channels unchanged', () => {
    expect(channelWebhookAllowed(event('UAPPROVED'), policy)).toBe(true)
    expect(channelWebhookAllowed(event('UOTHER'), policy)).toBe(false)
    expect(channelWebhookAllowed(event(undefined), policy)).toBe(false)
    expect(channelWebhookAllowed(event('UOTHER', 'COTHER'), policy)).toBe(true)
    expect(channelWebhookAllowed({ ...event('UAPPROVED'), team_id: 'TOTHER' }, policy)).toBe(false)
  })
  it('blocks follow-ups, rich message mentions, file events and edits before SDK dispatch', () => {
    for (const type of ['message', 'app_mention', 'file_shared']) {
      expect(isAllowedSlackWebhookBody(JSON.stringify(event('UOTHER', 'CPROTECTED', type)), options, logger)).toBe(false)
    }
    const edited = event('UAPPROVED', 'CPROTECTED', 'message')
    expect(channelWebhookAllowed({ ...edited, event: { ...edited.event, subtype: 'message_changed', message: { user: 'UAPPROVED' } } }, policy)).toBe(false)
  })
  it('does not grant Slack bots an exception for the separate provider webhook', () => {
    const bot = event('UAPPROVED')
    expect(channelWebhookAllowed({ ...bot, event: { ...bot.event, bot_id: 'BBOT' } }, policy)).toBe(false)
  })
  it('checks workflow buttons, conflicting destinations and slash commands', () => {
    const action = { type: 'block_actions', team: { id: 'THOME' }, user: { id: 'UOTHER' }, channel: { id: 'CPROTECTED' } }
    expect(channelWebhookAllowed(action, policy)).toBe(false)
    expect(channelWebhookAllowed({ ...action, user: { id: 'UAPPROVED' } }, policy)).toBe(true)
    expect(channelWebhookAllowed({ ...action, channel: { id: 'COTHER' }, container: { channel_id: 'CPROTECTED' } }, policy)).toBe(false)
    expect(channelWebhookAllowed({ ...action, channel: undefined }, policy)).toBe(false)
    const form = new URLSearchParams({ command: '/stop', team_id: 'THOME', channel_id: 'CPROTECTED', user_id: 'UOTHER' })
    expect(isAllowedSlackWebhookBody(form.toString(), options, logger)).toBe(false)
  })
  it('rechecks the normalized actor before message handoff', async () => {
    const message = { threadId: 'slack:CPROTECTED:1700000000.000001', author: { userId: 'UOTHER', isBot: false }, raw: { team: 'THOME' } } as Message
    expect(await isAllowedSlackMessage(message, options, logger)).toBe(false)
    message.author.userId = 'UAPPROVED'
    expect(await isAllowedSlackMessage(message, options, logger)).toBe(true)
  })
})

import { timingSafeEqual } from 'node:crypto'
import type { Hono } from 'hono'
import type { Message, StateAdapter } from 'chat'
import { forwardToSessionApi, serializeMessage } from './session-api'
import type { SlackbotV2ApiMessage, SlackbotV2Options } from './types'

/** Authenticated Slack operations for allowlisted durable workflows. */
export function mountWorkflowSlackTransport(
  app: Hono,
  options: SlackbotV2Options,
  state: StateAdapter,
  getThread?: (id: string) => { allMessages: AsyncIterable<Message> }
): void {
  const config = options.slashCommands
  const fetchFn = options.fetch ?? fetch
  const slackApi = async (
    method: string,
    args: Record<string, unknown>
  ): Promise<Record<string, any>> => {
    const response = await fetchFn(
      new URL(method, options.slackApiUrl ?? 'https://slack.com/api/'),
      {
        method: 'POST',
        headers: {
          authorization: `Bearer ${options.botToken}`,
          'content-type': 'application/json'
        },
        body: JSON.stringify(args),
        signal: AbortSignal.timeout(10_000)
      }
    )
    const result = (await response.json()) as Record<string, any>
    if (!response.ok || result.ok !== true)
      throw new Error(
        `Slack ${method} failed: ${result.error ?? response.status}`
      )
    return result
  }
  const acceptedScope = (team: unknown, channel: unknown) =>
    typeof team === 'string' &&
    /^T[A-Z0-9]+$/.test(team) &&
    (!config?.teamId || team === config.teamId) &&
    typeof channel === 'string' &&
    /^[CG][A-Z0-9]+$/.test(channel)
  let identity: Promise<Record<string, any>> | undefined
  const botTeam = async () => {
    identity ??= slackApi('auth.test', {}).catch((error) => {
      identity = undefined
      throw error
    })
    return (await identity).team_id
  }

  app.post('/internal/workflow/slack', async (c) => {
    const expected = options.apiKey
    const supplied = Buffer.from(c.req.header('authorization') ?? '')
    const expectedBytes = Buffer.from(`Bearer ${expected}`)
    if (
      !expected ||
      supplied.length !== expectedBytes.length ||
      !timingSafeEqual(supplied, expectedBytes)
    )
      return c.text('Unauthorized', 401)
    let request: any
    try {
      request = await c.req.json()
    } catch {
      return c.text('Invalid request', 400)
    }
    if (!request || typeof request !== 'object')
      return c.text('Invalid request', 400)
    const { operation, args } = request
    if (!args || !acceptedScope(args.team_id, args.channel))
      return c.text('Forbidden', 403)
    const { team_id: team, ...slackArgs } = args
    if (team !== (await botTeam())) return c.text('Forbidden', 403)
    const membership = await slackApi('conversations.info', {
      channel: args.channel
    })
    if (!membership.channel?.is_member)
      return c.text('Bot must be a channel member', 403)
    if (operation === 'context') {
      if (
        !/^\d+\.\d+$/.test(args.thread_ts ?? '') ||
        typeof args.text !== 'string' ||
        typeof args.event_id !== 'string'
      )
        return c.text('Invalid context', 400)
      const threadId = `slack:${args.channel}:${args.thread_ts}`
      const message: SlackbotV2ApiMessage = {
        id: args.event_id,
        threadId,
        teamId: team,
        text: args.text,
        timestamp: new Date().toISOString(),
        author: {
          userId: args.creator_id,
          userName: args.creator_id,
          fullName: args.creator_id,
          isBot: false,
          isMe: false
        },
        isMention: false,
        attachments: [],
        raw: { channel: args.channel, team, user: args.creator_id }
      }
      await state.subscribe(threadId)
      const historyKey = `workflow-context-initialized:${threadId}`
      const messages: SlackbotV2ApiMessage[] = []
      if (getThread && !(await state.get(historyKey))) {
        for await (const reply of getThread(threadId).allMessages)
          messages.push(await serializeMessage(reply, options))
      }
      messages.push(message)
      await forwardToSessionApi(options, {
        threadId,
        messages,
        afterEventId: 0,
        openStream: false,
        onEventId: () => {}
      })
      await state.set(historyKey, true)
      return c.json({ ok: true, thread_id: threadId })
    }
    const methods: Record<string, string> = {
      post: 'chat.postMessage',
      update: 'chat.update',
      ephemeral: 'chat.postEphemeral',
      user: 'users.info',
      channel: 'conversations.info',
      members: 'conversations.members',
      replies: 'conversations.replies',
      history: 'conversations.history',
      permalink: 'chat.getPermalink'
    }
    const method = methods[operation]
    if (typeof method !== 'string') return c.text('Unsupported operation', 400)
    try {
      return c.json(await slackApi(method, slackArgs))
    } catch {
      return c.json({ ok: false, error: 'slack_delivery_failed' }, 502)
    }
  })
}

import type { Logger, Message } from 'chat'
import type { JsonObject, JsonValue, SlackbotV2Options } from './types'
import { isJsonObject, stringValue } from './utils'

type RawSlackBotProfile = {
  app_id?: JsonValue
  id?: JsonValue
  user_id?: JsonValue
}

type RawSlackEvent = {
  app_id?: JsonValue
  bot_id?: JsonValue
  bot_profile?: RawSlackBotProfile
  channel?: JsonValue
  channel_type?: JsonValue
  source_team?: JsonValue
  subtype?: JsonValue
  team?: JsonValue
  team_id?: JsonValue
  text?: JsonValue
  thread_ts?: JsonValue
  ts?: JsonValue
  type?: JsonValue
  user?: JsonValue
  user_team?: JsonValue
}

type RawSlackEnvelope = {
  event?: JsonValue
  event_id?: JsonValue
  api_app_id?: JsonValue
  team_id?: JsonValue
  type?: JsonValue
}

type SlackEventLogDetails = JsonObject & {
  app_id?: string
  bot_id?: string
  channel_id?: string
  channel_type?: string
  event_id?: string
  event_subtype?: string
  event_type?: string
  is_mention?: boolean
  message_id?: string
  message_ts?: string
  raw_text_mentions_bot?: boolean
  team_id?: string
  thread_id?: string
  thread_ts?: string
  user_id?: string
}

export function isAllowedSlackWebhookBody(
  rawBody: string,
  options: SlackbotV2Options,
  logger: Logger
): boolean {
  let payload: unknown
  try {
    payload = JSON.parse(rawBody)
  } catch {
    return true
  }
  if (!isRawSlackEnvelope(payload) || payload.type !== 'event_callback') return true
  const event = isRawSlackEvent(payload.event) ? payload.event : undefined
  if (!event) return true

  const allowedExternalTeamIds =
    options.allowedExternalTeamIds ?? splitEnvList(process.env.SLACKBOT_EXTERNAL_ORG_ALLOWLIST)
  const externalTeamId = externalSlackTeamIdForHome(stringValue(payload.team_id), event)
  if (externalTeamId && !new Set(allowedExternalTeamIds).has(externalTeamId)) {
    logger.warn('slackbotv2_event_ignored_external_org_not_allowlisted', {
      event_id: stringValue(payload.event_id),
      external_team_id: externalTeamId,
      team_id: stringValue(payload.team_id)
    })
    return false
  }
  return true
}

export function isAllowedSlackMessage(
  message: Message,
  options: SlackbotV2Options,
  logger: Logger
): boolean {
  const raw = isRawSlackEvent(message.raw) ? message.raw : undefined
  const allowedExternalTeamIds =
    options.allowedExternalTeamIds ?? splitEnvList(process.env.SLACKBOT_EXTERNAL_ORG_ALLOWLIST)
  const externalTeamId = raw ? externalSlackTeamId(raw) : undefined
  if (externalTeamId && !new Set(allowedExternalTeamIds).has(externalTeamId)) {
    logger.warn('slackbotv2_event_ignored_external_org_not_allowlisted', {
      ...slackMessageLogDetails(message),
      external_team_id: externalTeamId,
      message_id: message.id,
      thread_id: message.threadId
    })
    return false
  }

  const triggerBotAllowlist =
    options.triggerBotAllowlist ?? splitEnvList(process.env.SLACKBOT_TRIGGER_BOT_ALLOWLIST)
  const botAuthored = message.author.isBot === true || (raw ? isBotAuthoredSlackEvent(raw) : false)
  if (botAuthored && !(raw && isAllowedTriggerBotMessage(raw, triggerBotAllowlist))) {
    logger.warn('slackbotv2_event_ignored_bot_not_allowlisted', {
      ...slackMessageLogDetails(message),
      message_id: message.id,
      thread_id: message.threadId
    })
    return false
  }

  return true
}

export function slackWebhookEventLogDetails(
  rawBody: string,
  botUserId?: string
): SlackEventLogDetails | null {
  let payload: unknown
  try {
    payload = JSON.parse(rawBody)
  } catch {
    return null
  }
  if (!isRawSlackEnvelope(payload) || payload.type !== 'event_callback') return null
  const event = isRawSlackEvent(payload.event) ? payload.event : undefined
  if (!event) return null
  return {
    app_id: stringValue(event.app_id) ?? stringValue(payload.api_app_id),
    bot_id: stringValue(event.bot_id) ?? stringValue(event.bot_profile?.id),
    channel_id: stringValue(event.channel),
    channel_type: stringValue(event.channel_type),
    event_id: stringValue(payload.event_id),
    event_subtype: stringValue(event.subtype),
    event_type: stringValue(event.type),
    message_ts: stringValue(event.ts),
    raw_text_mentions_bot: slackTextMentionsBot(stringValue(event.text), botUserId),
    team_id: stringValue(payload.team_id) ?? stringValue(event.team_id) ?? stringValue(event.team),
    thread_ts: stringValue(event.thread_ts),
    user_id: stringValue(event.user) ?? stringValue(event.bot_profile?.user_id)
  }
}

export function shouldLogSlackWebhookEvent(details: SlackEventLogDetails | null): boolean {
  if (!details) return false
  return Boolean(
    details.event_type === 'app_mention'
      || details.raw_text_mentions_bot === true
      || details.channel_type === 'im'
      || details.channel_type === 'mpim'
  )
}

export function slackMessageLogDetails(message: Message): SlackEventLogDetails {
  const raw = isRawSlackEvent(message.raw) ? message.raw : undefined
  return {
    app_id: raw ? stringValue(raw.app_id) ?? stringValue(raw.bot_profile?.app_id) : undefined,
    bot_id: raw ? stringValue(raw.bot_id) ?? stringValue(raw.bot_profile?.id) : undefined,
    channel_id: raw ? stringValue(raw.channel) : undefined,
    channel_type: raw ? stringValue(raw.channel_type) : undefined,
    event_subtype: raw ? stringValue(raw.subtype) : undefined,
    event_type: raw ? stringValue(raw.type) : undefined,
    is_mention: message.isMention === true,
    message_id: message.id,
    message_ts: raw ? stringValue(raw.ts) : undefined,
    team_id: raw ? stringValue(raw.team_id) ?? stringValue(raw.team) : undefined,
    thread_id: message.threadId,
    thread_ts: raw ? stringValue(raw.thread_ts) : undefined,
    user_id: message.author.userId || (raw ? stringValue(raw.user) : undefined)
  }
}

function externalSlackTeamId(event: RawSlackEvent): string | undefined {
  return externalSlackTeamIdForHome(stringValue(event.team_id), event)
}

function externalSlackTeamIdForHome(
  homeTeamId: string | undefined,
  event: RawSlackEvent
): string | undefined {
  if (!homeTeamId) return undefined
  for (const candidate of [event.user_team, event.source_team, event.team]) {
    const teamId = stringValue(candidate)
    if (teamId && teamId !== homeTeamId) return teamId
  }
  return undefined
}

function isBotAuthoredSlackEvent(event: RawSlackEvent): boolean {
  return Boolean(event.bot_id || event.bot_profile || event.subtype === 'bot_message')
}

function isAllowedTriggerBotMessage(
  event: RawSlackEvent,
  allowlist: readonly string[] | undefined
): boolean {
  if (!allowlist?.length) return false
  const appIds = normalizedIdentifierSet(stringValue(event.app_id), stringValue(event.bot_profile?.app_id))
  const botIds = normalizedIdentifierSet(stringValue(event.bot_id), stringValue(event.bot_profile?.id))
  const botUserIds = normalizedIdentifierSet(
    stringValue(event.user),
    stringValue(event.bot_profile?.user_id)
  )
  const anyIds = new Set([...appIds, ...botIds, ...botUserIds])

  for (const entry of allowlist) {
    const parsed = parseTriggerBotAllowlistEntry(entry)
    if (!parsed) continue
    if (parsed.kind === 'app' && appIds.has(parsed.value)) return true
    if (parsed.kind === 'bot' && botIds.has(parsed.value)) return true
    if (parsed.kind === 'user' && botUserIds.has(parsed.value)) return true
    if (parsed.kind === 'any' && anyIds.has(parsed.value)) return true
  }
  return false
}

function normalizedIdentifierSet(...values: Array<string | undefined>): Set<string> {
  return new Set(values.map(value => value?.trim()).filter((value): value is string => Boolean(value)))
}

function parseTriggerBotAllowlistEntry(
  entry: string
): { kind: 'app' | 'bot' | 'user' | 'any'; value: string } | null {
  const trimmed = entry.trim()
  if (!trimmed) return null
  const prefixed = /^(app|bot|user):(.+)$/i.exec(trimmed)
  if (!prefixed) return { kind: 'any', value: trimmed }
  const kind = prefixed[1]
  const value = prefixed[2]?.trim()
  if (!kind || !value) return null
  return { kind: kind.toLowerCase() as 'app' | 'bot' | 'user', value }
}

function splitEnvList(value: string | undefined): string[] {
  return (value ?? '')
    .split(/[\s,]+/)
    .map(part => part.trim())
    .filter(Boolean)
}

function slackTextMentionsBot(text: string | undefined, botUserId: string | undefined): boolean {
  if (!text || !botUserId) return false
  return text.includes(`<@${botUserId}>`)
}

function isRawSlackEvent(value: unknown): value is RawSlackEvent {
  return isJsonObject(value) && (value.bot_profile === undefined || isJsonObject(value.bot_profile))
}

function isRawSlackEnvelope(value: unknown): value is RawSlackEnvelope {
  return isJsonObject(value)
}

/** Trusted ingress policy, independent of model instructions or sandbox state. */
export type ChannelUserPolicy = Readonly<Record<string, {
  team_id: string
  allowed_user_ids: readonly string[]
}>>

export function parseChannelUserPolicy(raw: string | undefined): ChannelUserPolicy {
  if (raw === undefined) return {}
  let value: unknown
  try { value = JSON.parse(raw) } catch { throw new Error('Invalid SLACKBOTV2_CHANNEL_USER_POLICY JSON') }
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('Invalid channel user policy')
  for (const [channel, rule] of Object.entries(value)) {
    if (!/^[CG][A-Z0-9]+$/.test(channel) || !rule || typeof rule !== 'object' || Array.isArray(rule)
      || Object.keys(rule).sort().join(',') !== 'allowed_user_ids,team_id'
      || typeof rule.team_id !== 'string' || !/^T[A-Z0-9]+$/.test(rule.team_id)
      || !Array.isArray(rule.allowed_user_ids) || rule.allowed_user_ids.length === 0
      || rule.allowed_user_ids.some((id: unknown) => typeof id !== 'string' || !/^[UW][A-Z0-9]+$/.test(id))
      || new Set(rule.allowed_user_ids).size !== rule.allowed_user_ids.length) {
      throw new Error('Invalid channel user policy rule')
    }
  }
  return value as ChannelUserPolicy
}

export function channelUserAllowed(policy: ChannelUserPolicy | undefined,
  channel: unknown, team: unknown, user: unknown): boolean {
  if (typeof channel !== 'string' || !policy || !Object.hasOwn(policy, channel)) return true
  const rule = policy[channel]!
  return team === rule.team_id && typeof user === 'string' && rule.allowed_user_ids.includes(user)
}

function object(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : {}
}

/** Filter only; Slack's SDK must still verify the original request before accepting it. */
export function channelWebhookAllowed(payload: Record<string, unknown>, policy: ChannelUserPolicy | undefined): boolean {
  if (!policy || !Object.keys(policy).length) return true
  const event = object(payload.event)
  const channel = object(payload.channel)
  const container = object(payload.container)
  const team = object(payload.team)
  const user = object(payload.user)
  const message = object(event.message)
  // Check every advertised destination, so conflicting channel fields cannot
  // make the filter inspect one channel while the SDK acts on another.
  const channels = [event.channel, event.channel_id, payload.channel_id, channel.id, container.channel_id]
  const actor = payload.type === 'event_callback' ? event.user : (user.id ?? payload.user_id)
  const teamId = payload.team_id ?? team.id
  for (const id of channels) {
    if (!channelUserAllowed(policy, id, teamId, actor)) return false
    // Do not use the original message author to authorize someone else's edit.
    // Edited/deleted events cannot start or repair protected-channel executions.
    if (typeof id === 'string' && Object.hasOwn(policy, id)
      && (event.subtype === 'message_changed' || event.subtype === 'message_deleted'
        || event.bot_id || message.bot_id)) return false
  }
  // Native workflow buttons always have a channel. A missing destination must
  // not bypass the channel gate and then resolve to a protected workflow.
  if (payload.type === 'block_actions' && !channels.some(id => typeof id === 'string' && /^[CGD]/.test(id))) return false
  return true
}

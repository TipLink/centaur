import type { SlackbotV2Options } from './types'
import { parseSlackWebhookPayload } from './slack-events'

/** Forward signed bytes to a trusted ownership service before any SDK effects. */
export async function admitSlackThread(request: Request, body: string, options: SlackbotV2Options): Promise<boolean> {
  if (!options.threadAdmissionUrl) return true
  const payload = parseSlackWebhookPayload(body)
  const event = payload?.event as Record<string, unknown> | undefined
  const channel = payload?.channel as Record<string, unknown> | undefined
  const container = payload?.container as Record<string, unknown> | undefined
  const channels = [event?.channel, event?.channel_id, payload?.channel_id, channel?.id, container?.channel_id]
  if (!channels.some(id => typeof id === 'string' && Object.hasOwn(options.channelUserPolicy ?? {}, id))) return true
  // Channel-level controls/buttons cannot establish or switch a thread owner.
  if (payload?.type !== 'event_callback') return false
  const response = await (options.fetch ?? fetch)(options.threadAdmissionUrl, {
    method: 'POST', redirect: 'error', signal: AbortSignal.timeout(2000),
    headers: {
      'content-type': request.headers.get('content-type') ?? 'application/json',
      'x-slack-signature': request.headers.get('x-slack-signature') ?? '',
      'x-slack-request-timestamp': request.headers.get('x-slack-request-timestamp') ?? '',
      // Preserve the explicit Socket Mode rejection at the trusted service.
      ...(request.headers.has('x-slack-socket-token') ? { 'x-slack-socket-token': 'reject' } : {})
    }, body
  })
  if (!response.ok) throw new Error('Thread ownership unavailable')
  const result: unknown = await response.json()
  if (!result || typeof result !== 'object' || !('allowed' in result) || typeof result.allowed !== 'boolean') {
    throw new Error('Invalid thread ownership response')
  }
  return result.allowed
}

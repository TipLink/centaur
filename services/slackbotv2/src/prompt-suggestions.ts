import { randomUUID } from 'node:crypto'
import { assertSlackOk, callSlackApi } from '@chat-adapter/slack/api'
import type { StateAdapter } from 'chat'
import { slackApiTimeoutMs, withSlackApiTimeout } from './session-api'
import type { SlackbotV2Options, SlackbotV2RendererSource } from './types'
import { errorMessage, isJsonObject } from './utils'

const STATE_TTL_MS = 2 * 24 * 60 * 60 * 1000
const LOCK_TTL_MS = 5_000
const WRITE_TIMEOUT_MS = 2_000
const METHOD = 'assistant.threads.setSuggestedPrompts'

type Owner = {
  threadId: string
  messageId: string
  revision: string
  executionId?: string
  harnessType?: string
  stopped?: boolean
  eventId?: number
  suggestion?: string
  publishedRevision?: string
}

type KeepAlive = (promise: Promise<unknown>) => void

/** Agent-view prompts belong to the DM, not a particular message or thread. */
function channelFor(threadId: string, options: SlackbotV2Options): string | undefined {
  if (!options.promptSuggestionsEnabled || !options.agentViewEnabled || !options.state) return
  return /^slack:(D[^:]+):[^:]+$/.exec(threadId)?.[1]
}

function key(channel: string): string { return `slackbotv2:prompt-suggestions:${channel}` }

/** Called after Slack admission, including for a steer or stop. */
export async function beginPromptSuggestionMessage(
  threadId: string,
  messageId: string,
  options: SlackbotV2Options,
  keepAlive: KeepAlive
): Promise<void> {
  const channel = channelFor(threadId, options)
  if (!channel || !/^\d+\.\d+$/.test(messageId)) return
  await bestEffort(options, async () => {
    const changed = await withOwner(options.state!, channel, async current => {
      // Slack retries and concurrent threads must not put an older DM back in charge.
      if (current && Number(current.messageId) >= Number(messageId)) return false
      await options.state!.set(key(channel), {
        threadId, messageId, revision: randomUUID()
      } satisfies Owner, STATE_TTL_MS)
      return true
    })
    if (changed) keepAlive(reconcile(channel, options))
  })
}

export async function bindPromptSuggestionExecution(
  threadId: string,
  messageId: string,
  executionId: string,
  harnessType: string | undefined,
  options: SlackbotV2Options
): Promise<void> {
  const channel = channelFor(threadId, options)
  if (!channel) return
  await bestEffort(options, () => withOwner(options.state!, channel, async current => {
    if (current?.threadId !== threadId || current.messageId !== messageId || current.stopped) return
    await options.state!.set(key(channel), { ...current, executionId, harnessType }, STATE_TTL_MS)
  }))
}

export async function stopPromptSuggestions(
  threadId: string,
  options: SlackbotV2Options,
  keepAlive: KeepAlive
): Promise<void> {
  const channel = channelFor(threadId, options)
  if (!channel) return
  await bestEffort(options, async () => {
    const changed = await withOwner(options.state!, channel, async current => {
      if (current?.threadId !== threadId || current.stopped) return false
      await options.state!.set(key(channel), {
        threadId, messageId: current.messageId, stopped: true, revision: randomUUID()
      } satisfies Owner, STATE_TTL_MS)
      return true
    })
    if (changed) keepAlive(reconcile(channel, options))
  })
}

/** Consume the optional notification without delaying answer rendering. */
export async function* withPromptSuggestions(
  stream: AsyncIterable<SlackbotV2RendererSource>,
  threadId: string,
  messageId: string,
  executionId: string | undefined,
  options: SlackbotV2Options,
  keepAlive: KeepAlive
): AsyncIterable<SlackbotV2RendererSource> {
  for await (const event of stream) {
    const payload = suggestionPayload(event)
    if (payload) {
      const channel = channelFor(threadId, options)
      const eventId = 'eventId' in event ? event.eventId : undefined
      if (channel && executionId && typeof eventId === 'number') {
        keepAlive(bestEffort(options, async () => {
          const changed = await withOwner(options.state!, channel, async current => {
            if (current?.threadId !== threadId || current.messageId !== messageId
              || current.stopped
              || current.executionId !== executionId || current.harnessType !== 'claudecode'
              || (current.eventId ?? -1) > eventId) return false
            // Recovered output may replay a notification whose Slack write failed.
            // Keep its desired revision and retry only while it remains unpublished.
            if (current.eventId === eventId) {
              return current.publishedRevision !== current.revision
            }
            await options.state!.set(key(channel), {
              ...current, eventId, suggestion: payload, revision: randomUUID()
            } satisfies Owner, STATE_TTL_MS)
            return true
          })
          if (changed) await reconcile(channel, options)
        }))
      }
      continue
    }
    yield event
  }
}

function suggestionPayload(event: SlackbotV2RendererSource): string | undefined {
  const kind = 'eventKind' in event ? event.eventKind : event.event
  if (kind !== 'session.output.line' || typeof event.data !== 'string') return
  let payload: unknown
  try { payload = JSON.parse(event.data) } catch { return }
  if (!isJsonObject(payload) || payload.method !== 'turn/promptSuggestion'
    || !isJsonObject(payload.params)) return
  const suggestion = payload.params.suggestion
  if (typeof suggestion !== 'string') return
  const text = suggestion.trim()
  if (!text || [...text].length > 500 || /[\u0000-\u001f\u007f-\u009f]/.test(text)) return
  return text
}

async function withOwner<T>(
  state: StateAdapter,
  channel: string,
  operation: (current: Owner | null) => Promise<T>
): Promise<T | undefined> {
  let lock = await state.acquireLock(`${key(channel)}:lock`, LOCK_TTL_MS)
  for (let attempt = 0; !lock && attempt < 20; attempt++) {
    await new Promise(resolve => setTimeout(resolve, 25))
    lock = await state.acquireLock(`${key(channel)}:lock`, LOCK_TTL_MS)
  }
  if (!lock) return
  try { return await operation(await state.get<Owner>(key(channel))) }
  finally { await state.releaseLock(lock) }
}

async function reconcile(channel: string, options: SlackbotV2Options): Promise<void> {
  await bestEffort(options, async () => {
    let force = false
    for (let attempt = 0; attempt < 3; attempt++) {
      const current = await options.state!.get<Owner>(key(channel))
      if (!current || (!force && current.publishedRevision === current.revision)) return
      let succeeded = false
      const controller = new AbortController()
      try {
        const result = await withSlackApiTimeout({
          ...options, slackApiTimeoutMs: Math.min(slackApiTimeoutMs(options), WRITE_TIMEOUT_MS)
        }, METHOD, () => callSlackApi(METHOD, {
          channel_id: channel,
          // thread_ts silently fails in Slack's Agent view. Empty prompts clear
          // our previous suggestions; rejection is harmless UI-only failure.
          prompts: current.suggestion
            ? [{ title: current.suggestion, message: current.suggestion }]
            : []
        }, {
          apiUrl: options.slackApiUrl,
          token: options.botToken,
          fetch: ((input: RequestInfo | URL, init?: RequestInit) => (options.fetch ?? fetch)(
            input, { ...init, signal: controller.signal }
          )) as typeof fetch
        }))
        assertSlackOk(METHOD, result)
        succeeded = true
      } catch (error) {
        warn(options, error)
      } finally {
        controller.abort()
      }
      const latest = await withOwner(options.state!, channel, async owner => {
        if (succeeded && owner?.revision === current.revision) {
          await options.state!.set(key(channel), {
            ...owner, publishedRevision: owner.revision
          } satisfies Owner, STATE_TTL_MS)
        }
        return owner
      })
      if (!latest || latest.revision === current.revision) return
      // A slower old write may have landed after the latest clear/suggestion.
      // Reapply the current desired value even if another worker published it.
      force = true
    }
  })
}

async function bestEffort(options: SlackbotV2Options, operation: () => Promise<unknown>): Promise<void> {
  try { await operation() } catch (error) { warn(options, error) }
}

function warn(options: SlackbotV2Options, error: unknown): void {
  options.logger?.warn('slackbotv2_prompt_suggestions_failed', { error: errorMessage(error) })
}

import { describe, expect, test } from 'bun:test'
import { createMemoryState } from '@chat-adapter/state-memory'
import {
  beginPromptSuggestionMessage, bindPromptSuggestionExecution, stopPromptSuggestions,
  withPromptSuggestions
} from '../src/prompt-suggestions'
import type { JsonObject, SlackbotV2Options, SlackbotV2RendererSource } from '../src/types'

const THREAD = 'slack:D1:1700000000.000001'
const MESSAGE = '1700000000.000001'

function event(suggestion = 'Run the focused tests', eventId = 10): SlackbotV2RendererSource {
  return {
    eventKind: 'session.output.line', eventId,
    data: JSON.stringify({ method: 'turn/promptSuggestion', params: {
      threadId: 'native-thread', turnId: 'native-turn', suggestion
    } })
  }
}

function harness(overrides: Partial<SlackbotV2Options> = {}) {
  const state = createMemoryState()
  const calls: JsonObject[] = []
  const applied: JsonObject[] = []
  const pending: Promise<unknown>[] = []
  let fail = false
  let holdSuggestion: Promise<void> | undefined
  const options: SlackbotV2Options = {
    apiUrl: 'http://session.test', slackApiUrl: 'http://slack.test/api/',
    botToken: 'test-token', signingSecret: 'test-signing-secret',
    state, agentViewEnabled: true, promptSuggestionsEnabled: true,
    fetch: async (input, init) => {
      expect(String(input)).toBe('http://slack.test/api/assistant.threads.setSuggestedPrompts')
      const form = new URLSearchParams(String(init?.body))
      const body: JsonObject = {
        channel_id: form.get('channel_id'), prompts: JSON.parse(form.get('prompts')!)
      }
      expect(form.has('thread_ts')).toBe(false)
      calls.push(body)
      if (Array.isArray(body.prompts) && body.prompts.length) await holdSuggestion
      if (fail) return Response.json({ ok: false, error: 'static_prompts_configured' })
      applied.push(body)
      return Response.json({ ok: true })
    },
    ...overrides
  }
  const keepAlive = (promise: Promise<unknown>) => { pending.push(promise) }
  const drain = async () => {
    while (pending.length) await Promise.all(pending.splice(0))
  }
  const begin = async (thread = THREAD, message = MESSAGE, execution = 'execution-1', harnessType = 'claudecode') => {
    await state.connect()
    await beginPromptSuggestionMessage(thread, message, options, keepAlive)
    await bindPromptSuggestionExecution(thread, message, execution, harnessType, options)
    await drain()
  }
  const render = async (
    events = [event()], thread = THREAD, message = MESSAGE, execution = 'execution-1'
  ) => {
    async function* source() { yield* events }
    const output: SlackbotV2RendererSource[] = []
    for await (const item of withPromptSuggestions(source(), thread, message, execution, options, keepAlive)) {
      output.push(item)
    }
    return output
  }
  return {
    options, state, calls, applied, begin, render, drain, keepAlive,
    fail: (value = true) => { fail = value },
    hold: (promise: Promise<void>) => { holdSuggestion = promise }
  }
}

describe('native Slack prompt suggestions', () => {
  test('publishes one Claude suggestion through the native DM API and deduplicates replay', async () => {
    const h = harness()
    await h.begin()
    expect(h.calls).toEqual([{ channel_id: 'D1', prompts: [] }])
    expect(await h.render()).toEqual([])
    await h.drain()
    expect(h.calls.at(-1)).toEqual({ channel_id: 'D1', prompts: [
      { title: 'Run the focused tests', message: 'Run the focused tests' }
    ] })
    const count = h.calls.length
    await h.render()
    await h.drain()
    expect(h.calls).toHaveLength(count)
  })

  test('keeps Codex, channels, legacy view, and disabled deployments out', async () => {
    for (const [overrides, thread, harnessType] of [
      [{}, THREAD, 'codex'],
      [{}, 'slack:C1:1700000000.000001', 'claudecode'],
      [{ agentViewEnabled: false }, THREAD, 'claudecode'],
      [{ promptSuggestionsEnabled: false }, THREAD, 'claudecode']
    ] as const) {
      const h = harness(overrides)
      await h.begin(thread, MESSAGE, 'execution-1', harnessType)
      await h.render([event()], thread)
      await h.drain()
      expect(h.calls.filter(call => Array.isArray(call.prompts) && call.prompts.length)).toEqual([])
    }
  })

  test('recovery retries an unpublished suggestion and then deduplicates successful replay', async () => {
    const h = harness()
    await h.begin()
    h.fail()
    await h.render()
    await h.drain()
    expect(h.applied.at(-1)?.prompts).toEqual([])
    h.fail(false)
    await h.render()
    await h.drain()
    expect(h.applied.at(-1)?.prompts).toEqual([
      { title: 'Run the focused tests', message: 'Run the focused tests' }
    ])
    const count = h.calls.length
    await h.render()
    await h.drain()
    expect(h.calls).toHaveLength(count)
  })

  test('newer DM threads own the global prompt list; older events and retries cannot replace it', async () => {
    const h = harness()
    const newerMessage = '1700000001.000001'
    const newerThread = `slack:D1:${newerMessage}`
    await h.begin()
    await h.render()
    await h.drain()
    await h.begin(newerThread, newerMessage, 'execution-2')
    await h.render([event('Stale suggestion', 11)])
    await h.begin()
    await h.render([event('Review the new change', 12)], newerThread, newerMessage, 'execution-2')
    await h.drain()
    expect(h.applied.at(-1)?.prompts).toEqual([
      { title: 'Review the new change', message: 'Review the new change' }
    ])
    expect(JSON.stringify(h.calls)).not.toContain('Stale suggestion')
  })

  test('a steer or stop clears the old suggestion and rejects the old execution tail', async () => {
    const h = harness()
    await h.begin()
    await h.render()
    await h.drain()
    await beginPromptSuggestionMessage(THREAD, '1700000001.000001', h.options, h.keepAlive)
    await h.render([event('Old execution suggestion', 11)])
    await h.drain()
    expect(h.applied.at(-1)?.prompts).toEqual([])
    expect(JSON.stringify(h.calls)).not.toContain('Old execution suggestion')
  })

  test('a slow old write is followed by the latest clear', async () => {
    const h = harness()
    await h.begin()
    let release!: () => void
    h.hold(new Promise<void>(resolve => { release = resolve }))
    await h.render()
    for (let attempt = 0; h.calls.length < 2 && attempt < 100; attempt++) {
      await new Promise(resolve => setTimeout(resolve, 1))
    }
    expect(h.calls).toHaveLength(2)
    await beginPromptSuggestionMessage(THREAD, '1700000001.000001', h.options, h.keepAlive)
    for (let attempt = 0; h.applied.length < 2 && attempt < 100; attempt++) {
      await new Promise(resolve => setTimeout(resolve, 1))
    }
    release()
    await h.drain()
    expect(h.applied.at(-1)?.prompts).toEqual([])
  })

  test('native Stop clears the owner and rejects a late execution binding or suggestion', async () => {
    const h = harness()
    await h.begin()
    await h.render()
    await h.drain()
    await stopPromptSuggestions(THREAD, h.options, h.keepAlive)
    await bindPromptSuggestionExecution(THREAD, MESSAGE, 'execution-1', 'claudecode', h.options)
    await h.render([event('Late stopped suggestion', 11)])
    await h.drain()
    expect(h.applied.at(-1)?.prompts).toEqual([])
    expect(JSON.stringify(h.calls)).not.toContain('Late stopped suggestion')
  })

  test('a pending or failed Slack suggestion write never holds the answer stream', async () => {
    const h = harness()
    await h.begin()
    let release!: () => void
    h.hold(new Promise<void>(resolve => { release = resolve }))
    const answer = { eventKind: 'session.output.line', eventId: 9, data: 'answer' }
    const terminal = { eventKind: 'session.output.line', eventId: 11, data: 'terminal' }
    expect(await h.render([answer, event(), terminal])).toEqual([answer, terminal])
    h.fail()
    release()
    await h.drain()
  })

  test('zero or invalid suggestions do not invent a replacement prompt', async () => {
    const h = harness()
    await h.begin()
    await h.render([])
    await h.render([event(''), event('x'.repeat(501)), event('two\nlines')])
    await h.drain()
    expect(h.calls).toEqual([{ channel_id: 'D1', prompts: [] }])
  })

  test('uses the producer character bound for Unicode suggestions', async () => {
    const h = harness()
    const suggestion = '🔎'.repeat(500)
    await h.begin()
    await h.render([event(suggestion)])
    await h.drain()
    expect(h.applied.at(-1)?.prompts).toEqual([{ title: suggestion, message: suggestion }])
  })

  test('bounds a stalled response body as well as the fetch', async () => {
    let signal: AbortSignal | null | undefined
    const h = harness({
      slackApiTimeoutMs: 10,
      fetch: async (_input, init) => {
        signal = init?.signal
        return new Response(new ReadableStream({ start() {} }))
      }
    })
    await h.begin()
    await h.render()
    await h.drain()
    expect(signal?.aborted).toBe(true)
  })
})

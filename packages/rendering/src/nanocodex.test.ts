import { describe, expect, test } from 'bun:test'
import type { ChatSDKStreamChunk } from './chat-sdk'
import {
  NanocodexRendererEventMapper,
  harnessToChatSdkStream,
  isNanocodexEvent
} from './nanocodex'

const native = (type: string, payload: Record<string, unknown>, seq = 1) => ({
  eventKind: 'session.output.line',
  data: JSON.stringify({
    protocol_version: 1,
    request_id: 'nano-session',
    seq,
    type,
    payload
  })
})

describe('NanocodexRendererEventMapper', () => {
  test('streams native assistant events and completes with the canonical answer', () => {
    const mapper = new NanocodexRendererEventMapper()
    expect(isNanocodexEvent(native('run.started', {}))).toBe(true)
    expect(mapper.process(native('assistant.delta', { text: 'hello' }, 2))).toEqual([
      { type: 'renderer.message.delta', delta: 'hello' }
    ])
    expect(mapper.process(native('assistant.message', { text: 'hello world' }, 3))).toEqual([
      { type: 'renderer.message.delta', delta: ' world' }
    ])
    expect(mapper.process(native('run.completed', {}, 4))).toEqual([
      {
        type: 'renderer.done',
        answerMarkdown: 'hello world',
        streamFinalUpdates: true,
        threadId: 'nano-session'
      }
    ])
  })

  test('renders completed commentary as progress before streaming the final answer', () => {
    const mapper = new NanocodexRendererEventMapper()
    expect(
      mapper.process(
        native('assistant.delta', {
          item_id: 'commentary-1',
          phase: 'commentary',
          text: 'I’ll verify.'
        })
      )
    ).toEqual([])
    expect(
      mapper.process(
        native('assistant.message', {
          item_id: 'commentary-1',
          phase: 'commentary',
          text: 'I’ll verify.'
        }, 2)
      )
    ).toEqual([
      {
        type: 'renderer.task.update',
        task: {
          id: 'commentary-1',
          title: 'Thinking',
          status: 'complete',
          details: [{ type: 'text', text: 'I’ll verify.' }]
        }
      }
    ])
    expect(
      mapper.process(
        native('assistant.delta', {
          item_id: 'answer-1',
          phase: 'final_answer',
          text: 'Done.'
        }, 3)
      )
    ).toEqual([{ type: 'renderer.message.delta', delta: 'Done.' }])
    expect(
      mapper.process(
        native('assistant.message', {
          phase: 'final_answer',
          text: 'Done.'
        }, 4)
      )
    ).toEqual([])
    expect(mapper.process(native('run.completed', {}, 5))).toEqual([
      {
        type: 'renderer.done',
        answerMarkdown: 'Done.',
        streamFinalUpdates: true,
        threadId: 'nano-session'
      }
    ])
  })

  test('renders native tool lifecycle without an app-server conversion', () => {
    const mapper = new NanocodexRendererEventMapper()
    expect(
      mapper.process(
        native('tool.call', { call_id: 'call-1', tool: 'shell', arguments: { cmd: 'pwd' } })
      )[0]
    ).toMatchObject({
      type: 'renderer.task.update',
      task: { id: 'call-1', title: 'shell', status: 'in_progress' }
    })
    expect(
      mapper.process(
        native('tool.result', { call_id: 'call-1', tool: 'shell', status: 'completed', result: 'ok' })
      )[0]
    ).toMatchObject({
      type: 'renderer.task.update',
      task: { id: 'call-1', title: 'shell', status: 'complete' }
    })
  })

  test('keeps commentary and tools interleaved ahead of the final answer', async () => {
    async function* events() {
      yield native('assistant.delta', {
        item_id: 'commentary-1', phase: 'commentary', text: 'Checking.'
      }, 1)
      yield native('assistant.message', {
        item_id: 'commentary-1', phase: 'commentary', text: 'Checking.'
      }, 2)
      yield native('tool.call', { call_id: 'call-1', tool: 'shell', arguments: 'pwd' }, 3)
      yield native('tool.result', {
        call_id: 'call-1', tool: 'shell', status: 'completed', result: '/workspace'
      }, 4)
      yield native('assistant.delta', {
        item_id: 'commentary-2', phase: 'commentary', text: 'Found it.'
      }, 5)
      yield native('assistant.message', {
        item_id: 'commentary-2', phase: 'commentary', text: 'Found it.'
      }, 6)
      yield native('assistant.delta', {
        item_id: 'answer-1', phase: 'final_answer', text: 'Done.'
      }, 7)
      yield native('assistant.message', {
        item_id: 'answer-1', phase: 'final_answer', text: 'Done.'
      }, 8)
      yield native('run.completed', {}, 9)
    }

    const chunks: ChatSDKStreamChunk[] = []
    for await (const chunk of harnessToChatSdkStream(events())) chunks.push(chunk)
    expect(chunks.map(chunk => chunk.type === 'task_update'
      ? `${chunk.id}:${chunk.status}`
      : `${chunk.type}:${chunk.type === 'markdown_text' ? chunk.text : chunk.title}`
    )).toEqual([
      'commentary-1:complete',
      'call-1:in_progress',
      'call-1:complete',
      'commentary-2:complete',
      'markdown_text:Done.'
    ])
  })

  test('carries run.error into the terminal failure', () => {
    const mapper = new NanocodexRendererEventMapper()
    expect(mapper.process(native('run.error', { message: 'proxy refused' }))).toEqual([])
    expect(mapper.process(native('run.failed', {}, 2))).toEqual([
      {
        type: 'renderer.done',
        answerMarkdown: undefined,
        error: 'proxy refused',
        streamFinalUpdates: true,
        threadId: 'nano-session'
      }
    ])
  })

  test('bounds the error of a failed run and of a failed session', () => {
    // All 19,479 characters of a failure used to reach `renderer.done`.
    const error = `proxy refused\n${'  at frame (internal detail)\n'.repeat(671)}`
    expect(error.length).toBeGreaterThan(19_000)
    const shown = `${error.replace(/\s+/g, ' ').slice(0, 497)}...`
    expect(shown).toHaveLength(500)

    const failedRun = new NanocodexRendererEventMapper()
    expect(failedRun.process(native('run.error', { message: error }))).toEqual([])
    expect(failedRun.process(native('run.failed', {}, 2))).toEqual([
      {
        type: 'renderer.done',
        // No answer was given, and none is made up from the error.
        answerMarkdown: undefined,
        error: shown,
        streamFinalUpdates: true,
        threadId: 'nano-session'
      }
    ])

    for (const eventKind of [
      'session.execution_failed',
      'session.stream_error',
      'session.stdout_pump_failed'
    ]) {
      const failedSession = new NanocodexRendererEventMapper()
      failedSession.process(native('assistant.delta', { text: 'partial' }))
      expect(failedSession.process({ eventKind, data: { error } })).toEqual([
        {
          type: 'renderer.done',
          answerMarkdown: 'partial',
          error: shown,
          streamFinalUpdates: true,
          threadId: 'nano-session'
        }
      ])
    }
  })

  test('reports nothing of the value a parse failure quotes', () => {
    const mapper = new NanocodexRendererEventMapper()
    const events = mapper.process({
      eventKind: 'session.execution_failed',
      data: {
        error: `JSON error: invalid type: string "START-SECRET${' internal detail'.repeat(1_200)}END-SECRET", expected a sequence`
      }
    })

    expect(events).toEqual([
      {
        type: 'renderer.done',
        answerMarkdown: undefined,
        error: 'JSON error: invalid type: string [redacted]',
        streamFinalUpdates: true,
        threadId: undefined
      }
    ])
  })

  test('closes the session with the bounded error', async () => {
    const closed: unknown[] = []
    const sources = (async function* () {
      yield native('run.error', { message: 'boom '.repeat(4_000) })
      yield native('run.failed', {}, 2)
    })()
    for await (const _ of harnessToChatSdkStream(sources, {
      onOutput: output => {
        if (output.type === 'chat.session.closed') closed.push(output.message)
      }
    })) {
      // Only the closing output matters here.
    }

    expect(closed).toEqual([
      { text: undefined, error: `${'boom '.repeat(100).slice(0, 497)}...` }
    ])
  })

  test('ends a cancelled run without a notice when silentInterrupt is set', () => {
    const mapper = new NanocodexRendererEventMapper({ silentInterrupt: true })
    expect(mapper.process(native('run.failed', { status: 'cancelled' }, 2))).toEqual([
      {
        type: 'renderer.done',
        answerMarkdown: '',
        interrupted: true,
        streamFinalUpdates: true,
        threadId: 'nano-session'
      }
    ])
  })

  test('renders a cancelled run through the stock interruption path', () => {
    const mapper = new NanocodexRendererEventMapper()
    expect(mapper.process(native('run.error', { message: 'turn cancelled' }))).toEqual([])
    expect(mapper.process(native('run.failed', { status: 'cancelled' }, 2))).toEqual([
      {
        type: 'renderer.done',
        answerMarkdown: 'Execution interrupted.',
        interrupted: true,
        streamFinalUpdates: true,
        threadId: 'nano-session'
      }
    ])
  })
})

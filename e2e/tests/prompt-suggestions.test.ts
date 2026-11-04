import { expect, test } from 'bun:test'
import { eventually, model, slack, turnTimeoutMs, USER_B } from '../lib'

// Run this suite with Slack Agent view and prompt suggestions enabled in the
// isolated stack. The ordinary suites retain the legacy DM configuration.
test.skipIf(process.env.E2E_PROMPT_SUGGESTIONS_ENABLED !== 'true')(
  'Claude native suggestions survive the durable stream and reach Slack Agent view',
  async () => {
    const suggestion = 'Show me the test results'
    const script = model.suggests('The change is ready to review.', suggestion)
    const thread = await slack.dm('--sonnet Run the command, then report the result.', script, { agentView: true })
    const turn = await thread.nextTurn()
    expect(turn.execution.status).toBe('completed')
    expect(turn.reply).toBe(script.text)
    expect(turn.sandbox.harness).toBe('claudecode')

    const output = (await thread.outputLines(turn.execution.execution_id)).flatMap(({ line }) => {
      try { return [JSON.parse(line)] } catch { return [] }
    })
    const suggestions = output.filter(event => event.method === 'turn/promptSuggestion')
    expect(suggestions).toHaveLength(1)
    expect(suggestions[0].params.suggestion).toBe(suggestion)
    const completion = output.find(event => event.method === 'turn/completed')
    expect(completion).toBeDefined()
    expect(suggestions[0].params.threadId).toBe(completion.params.threadId)
    expect(suggestions[0].params.turnId).toBe(completion.params.turn.id)
    expect(output.indexOf(suggestions[0])).toBeLessThan(output.indexOf(completion))

    const published = await eventually('the native Slack suggested prompt', turnTimeoutMs, async () =>
      (await slack.suggestedPrompts(thread.channel)).find(call => call.prompts[0]?.message === suggestion)
    )
    expect(published.prompts).toEqual([{ title: suggestion, message: suggestion }])
    // Agent view silently ignores thread-scoped prompts; this call belongs to
    // the DM's Messages surface and must omit thread_ts altogether.
    expect(published).not.toHaveProperty('thread_ts')
    expect(published.channel_id).toBe(thread.channel.id)

    const requests = await model.requests(script)
    expect(requests).toHaveLength(3)
    expect(requests.every(request => request.provider === 'anthropic' && request.model === 'claude-sonnet-5-5')).toBe(true)
    expect(requests[2]!.userText).toContain('[SUGGESTION MODE:')
    // The real CLI makes one native prediction request. Rendering the button
    // neither asks another model nor allocates another sandbox.
    expect(await thread.sandboxes()).toHaveLength(1)
    expect(await thread.executions()).toHaveLength(1)

    const previousWrites = (await slack.suggestedPrompts(thread.channel)).length
    const nextScript = model.says('A short answer with no suggestion.')
    const next = await slack.dm('--sonnet Answer briefly.', nextScript, { agentView: true })
    await eventually('the next DM clearing the old prompt', turnTimeoutMs, async () => {
      const calls = await slack.suggestedPrompts(thread.channel)
      return calls.length > previousWrites && calls.at(-1)?.prompts.length === 0 ? true : undefined
    })
    const nextTurn = await next.nextTurn()
    expect(nextTurn.execution.status).toBe('completed')
    expect(nextTurn.reply).toBe(nextScript.text)
    expect(await model.requests(nextScript)).toHaveLength(1)
    expect((await next.outputLines(nextTurn.execution.execution_id)).some(({ line }) =>
      line.includes('turn/promptSuggestion')
    )).toBe(false)
    expect((await slack.suggestedPrompts(thread.channel)).at(-1)?.prompts).toEqual([])
  },
  2 * turnTimeoutMs
)

test.skipIf(process.env.E2E_PROMPT_SUGGESTIONS_ENABLED !== 'true')(
  'a follow-up during Claude prediction finishes in the same execution',
  async () => {
    const staleSuggestion = 'Show the old result'
    const first = model.suggests('The first part is ready.', staleSuggestion, { suggestionDelayMs: 10_000 })
    const thread = await slack.dm('--sonnet Run the command, then report the result.', first, { as: USER_B, agentView: true })
    // The native tail is only two seconds; poll its provider request more
    // frequently than the ordinary one-second eventually helper.
    const deadline = Date.now() + turnTimeoutMs
    let predicting = false
    while (Date.now() < deadline) {
      predicting = (await model.requests(first)).some(request => request.userText.includes('[SUGGESTION MODE:'))
      if (predicting) break
      await Bun.sleep(25)
    }
    expect(predicting).toBe(true)
    const followUp = model.says('The follow-up requirement is complete.')
    await thread.interject('Also handle this follow-up requirement.', followUp, { as: USER_B })
    const turn = await thread.nextTurn()
    expect(turn.execution.status).toBe('completed')
    expect(turn.reply).toContain(followUp.text)
    expect(await thread.executions()).toHaveLength(1)
    expect(await thread.sandboxes()).toHaveLength(1)
    expect((await model.requests(followUp))[0]?.userMessage).toContain('Also handle this follow-up requirement.')
    expect((await thread.outputLines(turn.execution.execution_id)).some(({ line }) =>
      line.includes('turn/promptSuggestion') && line.includes(staleSuggestion)
    )).toBe(false)
    expect((await slack.suggestedPrompts(thread.channel)).some(call =>
      call.prompts.some(prompt => prompt.message === staleSuggestion)
    )).toBe(false)
  },
  2 * turnTimeoutMs
)

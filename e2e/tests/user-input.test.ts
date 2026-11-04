import { expect, test } from 'bun:test'
import { api, eventually, model, slack, turnTimeoutMs, USER, USER_B } from '../lib'

for (const harness of ['codex', 'claudecode']) {
  test(`${harness} asks two native questions and continues the same execution`, async () => {
    const script = model.asks([
      { id: 'project', header: 'Project', question: 'Which project should own this work?', options: [
        { label: 'API reliability', description: 'Improve the API service.' },
        { label: 'Mobile experience', description: 'Improve the mobile app.' }
      ] },
      { id: 'title', header: 'Title', question: 'Which title should the ticket use?', options: [
        { label: 'Improve retries', description: 'Focus on request retry behavior.' },
        { label: 'Improve monitoring', description: 'Focus on service visibility.' }
      ] }
    ], `The ${harness} task continued with the supplied answers.`)
    const selection = harness === 'claudecode' ? '--sonnet' : '--codex'
    const thread = await slack.mention(`${selection} Ask the scoped questions, then continue.`, script)
    const path = `/api/session/${encodeURIComponent(thread.key)}/user-input`
    const request = await eventually('a durable native question request', turnTimeoutMs, async () => {
      const response = await api.request('GET', path)
      return response.status === 200 ? response.body.requests?.[0] : undefined
    })
    expect(request.requester_user_id).toBe(USER.id)
    expect(request.questions).toHaveLength(2)
    const card = await eventually('the first question card', turnTimeoutMs, async () =>
      (await thread.questionCards())[0]
    )
    expect(card.text).toContain('Question 1 of 2')
    expect((await model.requests(script)).length).toBe(1)

    // Another channel member cannot supply the requester's decision.
    await slack.click(thread.channel, card.ts, 'centaur.user_input:0', { as: USER_B })
    expect((await slack.message(thread.channel, card.ts)).text).toContain('Question 1 of 2')
    await slack.click(thread.channel, card.ts, 'centaur.user_input:0')
    const second = await eventually('the second question on the same card', turnTimeoutMs, async () => {
      const message = await slack.message(thread.channel, card.ts)
      return message.text?.includes('Question 2 of 2') ? message : undefined
    })
    expect(second.ts).toBe(card.ts)
    expect((await model.requests(script)).length).toBe(1)
    expect((await api.ok('GET', `${path}/${request.request_id}`)).status).toBe('pending')

    await slack.click(thread.channel, card.ts, 'centaur.user_input:other')
    const view = await eventually('the typed-answer modal', turnTimeoutMs, async () => {
      const views = await slack.views()
      return views.find(view => String(view.view.private_metadata).includes(request.request_id))
    })
    const custom = 'Retry "429" responses without duplicate work'
    const submitted = await slack.submitView(view.id, { custom: { text: { value: custom } } })
    expect(submitted.status).toBe(200)
    expect(JSON.parse(submitted.body)).toEqual({ response_action: 'clear' })

    const turn = await thread.nextTurn()
    expect(turn.execution.status).toBe('completed')
    expect(turn.execution.execution_id).toBe(request.execution_id)
    expect(turn.reply).toBe(script.text)
    expect(await thread.executions()).toHaveLength(1)
    expect(await thread.questionCards()).toHaveLength(1)
    const resolved = await api.ok('GET', `${path}/${request.request_id}`)
    expect(resolved.status).toBe('resolved')
    expect(resolved.answers).toEqual({
      [request.questions[0].id]: { answers: ['API reliability'] },
      [request.questions[1].id]: { answers: [custom] }
    })
    const providerRequests = await model.requests(script)
    expect(providerRequests).toHaveLength(2)
    if (harness === 'claudecode') expect(providerRequests[0]!.model).toBe('claude-sonnet-5-5')
    expect(JSON.stringify(providerRequests[1].body)).toContain('API reliability')
    expect(JSON.stringify(providerRequests[1].body)).toContain('duplicate work')
  }, turnTimeoutMs)
}

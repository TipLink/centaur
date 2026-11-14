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
    const card = await eventually('the question card', turnTimeoutMs, async () =>
      (await thread.questionCards())[0]
    )
    expect(card.text).toContain('Answer these 2 questions to continue.')
    expect((await model.requests(script)).length).toBe(1)

    const custom = 'Retry "429" responses without duplicate work'
    const submit = 'centaur.user_input:submit'
    const first = { q0: { selected: { type: 'radio_buttons', selected_option: { value: '0' } } } }
    const both = { ...first, c1: { text: { type: 'plain_text_input', value: custom } } }
    // Another channel member cannot supply the requester's decision.
    await slack.click(thread.channel, card.ts, submit, { as: USER_B, stateValues: both })
    expect((await api.ok('GET', `${path}/${request.request_id}`)).status).toBe('pending')

    // One answered question is not enough: the same card asks for the rest.
    await slack.click(thread.channel, card.ts, submit, { stateValues: first })
    const reminded = await eventually('the card naming the unanswered question', turnTimeoutMs, async () => {
      const message = await slack.message(thread.channel, card.ts)
      return JSON.stringify(message.blocks).includes('Still needed: Title') ? message : undefined
    })
    expect(reminded.ts).toBe(card.ts)
    expect((await model.requests(script)).length).toBe(1)
    expect((await api.ok('GET', `${path}/${request.request_id}`)).status).toBe('pending')

    expect((await slack.click(thread.channel, card.ts, submit, { stateValues: both })).status).toBe(200)

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

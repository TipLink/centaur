import { assertSlackOk, callSlackApi, fetchSlackThreadReplies } from '@chat-adapter/slack/api'
import type { StateAdapter, Thread } from 'chat'
import {
  getSessionUserInput,
  SESSION_USER_INPUT_REQUESTED_EVENT,
  SESSION_USER_INPUT_RESOLVED_EVENT,
  submitSessionUserInput,
  withSlackApiTimeout
} from './session-api'
import type {
  JsonObject, SessionUserInputAnswers, SessionUserInputQuestion, SessionUserInputRequest,
  SlackbotV2Options, SlackbotV2RendererSource
} from './types'
import { errorMessage, isJsonObject, stringValue } from './utils'

const PREFIX = 'centaur.user_input:'
const MODAL_ID = `${PREFIX}answer`
const STATE_TTL_MS = 7 * 24 * 60 * 60 * 1000
const LOCK_TTL_MS = 120_000
const POST_RETRY_INTERVAL_MS = 30_000
const INTERACTION_ACK_TIMEOUT_MS = 2000

export class UserInputRenderError extends Error {
  constructor(cause: unknown) {
    super(`Could not render pending questions: ${errorMessage(cause)}`, { cause })
    this.name = 'UserInputRenderError'
  }
}

type QuestionCard = {
  request: SessionUserInputRequest
  threadId: string
  channel: string
  threadTs: string
  messageTs?: string
  postingAt?: number
  index: number
  answers: SessionUserInputAnswers
  closed?: boolean
}

type Choice = { request: string; index: number; choice: number | 'other' | 'edit'; channel_id?: string }

function cardKey(id: string): string { return `slackbotv2:user-input:${id}` }
function threadKey(id: string): string { return `slackbotv2:user-input-thread:${id}` }

/** Native requests are durable API events; Slack state only tracks their presentation. */
export async function renderSessionUserInput(
  thread: Thread,
  event: SlackbotV2RendererSource,
  options: SlackbotV2Options,
  keepAlive?: (promise: Promise<unknown>) => void
): Promise<boolean> {
  const close = async (id: string) => {
    const pending = retryCardUpdate(id, () => closeCard(id, options), options)
    if (!keepAlive) return pending
    keepAlive(pending.catch(error => options.logger?.warn('slackbotv2_user_input_close_failed', {
      request_id: id, error: errorMessage(error)
    })))
  }
  const kind = 'eventKind' in event ? event.eventKind : event.event
  if (kind !== SESSION_USER_INPUT_REQUESTED_EVENT && kind !== SESSION_USER_INPUT_RESOLVED_EVENT) {
    if (typeof kind === 'string' && /^session\.execution_(completed|cancelled|failed)$/.test(kind)) {
      const ids = await options.state?.get<string[]>(threadKey(thread.id)) ?? []
      for (const id of ids) await close(id)
    }
    return false
  }
  const data = 'data' in event ? event.data : undefined
  if (!isJsonObject(data) || typeof data.request_id !== 'string' || !options.state) return true
  if (kind === SESSION_USER_INPUT_RESOLVED_EVENT) {
    await close(data.request_id)
    return true
  }
  const requestId = data.request_id
  await withCardLock(options.state, requestId, async () => {
    const request = await getSessionUserInput(options, thread.id, requestId)
    if (request.status !== 'pending') return
    validateRequest(request)
    const coordinates = /^slack:([^:]+):([^:]+)$/.exec(thread.id)
    if (!coordinates) throw new Error('User input requires a Slack thread')
    let card = await options.state!.get<QuestionCard>(cardKey(requestId))
    if (!card) {
      card = {
        request, threadId: thread.id, channel: coordinates[1]!, threadTs: coordinates[2]!,
        index: 0, answers: Object.create(null) as SessionUserInputAnswers
      }
      await saveCard(card, options)
      const ids = await options.state!.get<string[]>(threadKey(thread.id)) ?? []
      await options.state!.set(threadKey(thread.id), [...new Set([...ids, requestId])].slice(-20), STATE_TTL_MS)
    }
    if (card.closed) return
    if (card.messageTs) {
      await updateCard(card, options)
      return
    }
    if (card.postingAt) {
      // A prior post may have succeeded before the process saved its timestamp.
      // Reconcile first; if absent, retry the same client message id at most
      // once per interval. Slack delivery is not an exactly-once boundary.
      card.messageTs = await findPostedCard(card, options)
      if (card.messageTs) {
        await saveCard(card, options)
        return
      }
      if (Date.now() - card.postingAt < POST_RETRY_INTERVAL_MS) {
        throw new Error('Question card delivery is still unconfirmed')
      }
    }
    card.postingAt = Date.now()
    await saveCard(card, options)
    const result = await slackCall(options, 'chat.postMessage', {
      channel: card.channel, thread_ts: card.threadTs,
      client_msg_id: requestId, ...cardMessage(card)
    })
    if (result.ok !== true) {
      // Slack definitively rejected the post. A later render can retry it.
      delete card.postingAt
      await saveCard(card, options)
      assertSlackOk('chat.postMessage', result)
    }
    if (typeof result.ts !== 'string') throw new Error('Slack question card has no timestamp')
    card.messageTs = result.ts
    await saveCard(card, options)
  })
  return true
}

/** Called only after Slack signature and external-team policy verification. */
export async function handleSessionUserInputInteraction(
  payload: Record<string, unknown> | null,
  options: SlackbotV2Options,
  stateReady?: Promise<unknown>,
  keepAlive?: (promise: Promise<unknown>) => void
): Promise<Response | null> {
  if (!payload) return null
  const view = isJsonObject(payload.view) ? payload.view : undefined
  const actions = Array.isArray(payload.actions) ? payload.actions : []
  const action = actions.find(item => isJsonObject(item)
    && typeof item.action_id === 'string' && item.action_id.startsWith(PREFIX))
  const modal = payload.type === 'view_submission' && view?.callback_id === MODAL_ID
  if (!modal && !action) return null
  if (!options.state) return new Response('Question storage unavailable', { status: 503 })
  const user = isJsonObject(payload.user) ? stringValue(payload.user.id) : undefined
  let choice: Choice
  try {
    const value = modal ? view?.private_metadata : isJsonObject(action) ? action.value : undefined
    choice = parseChoice(value)
  } catch {
    return new Response('Invalid question choice', { status: 400 })
  }
  const refresh = (card: QuestionCard) => {
    if (!keepAlive) return retryCardUpdate(choice.request, () => updateCard(card, options), options)
    // Re-read under the same lock after the handoff releases it, so a delayed
    // display update cannot overwrite a newer page or the resolved state.
    const update = retryCardUpdate(choice.request, () => withCardLock(options.state!, choice.request, async () => {
      const latest = await options.state!.get<QuestionCard>(cardKey(choice.request))
      if (latest) await updateCard(latest, options)
    }), options)
    keepAlive(update)
    return Promise.resolve()
  }
  const handoff = async () => {
    try {
      await stateReady
      return await withCardLock(options.state!, choice.request, async () => {
        const card = await options.state!.get<QuestionCard>(cardKey(choice.request))
        if (!card || !user || card.request.requester_user_id !== user) return unavailable(modal, view)
        if (modal && choice.channel_id !== card.channel) return unavailable(true, view)
        if (!modal) {
          const channel = isJsonObject(payload.channel) ? payload.channel.id : undefined
          const message = isJsonObject(payload.message) ? payload.message : undefined
          if (!message || channel !== card.channel || message.ts !== card.messageTs
            || (message.thread_ts ?? message.ts) !== card.threadTs) return unavailable(false)
        }
        if ((card.closed && card.request.status !== 'submitted') || choice.index !== card.index) {
          await refresh(card)
          return card.closed && card.request.status !== 'cancelled' && modal
            ? Response.json({ response_action: 'clear' }) : unavailable(modal, view)
        }
        const current = await getSessionUserInput(options, card.threadId, choice.request)
        if (current.execution_id !== card.request.execution_id || current.requester_user_id !== user) {
          return unavailable(modal, view)
        }
        if (current.status === 'resolved' || current.status === 'cancelled') {
          card.closed = true
          card.request = current
          await saveCard(card, options)
          await refresh(card)
          return current.status !== 'cancelled' && modal
            ? Response.json({ response_action: 'clear' }) : unavailable(modal, view)
        }
        if (current.status === 'submitted') {
          // The API persists answers before writing them to the harness. A
          // previous write may have failed or its HTTP response may be missing;
          // resubmit the immutable answer set under the same native request id.
          if (!current.answers) throw new Error('Submitted question has no saved answers')
          await submitSessionUserInput(options, card.threadId, current, current.answers, user)
          card.closed = true
          card.request = current
          await saveCard(card, options)
          await refresh(card)
          return modal ? Response.json({ response_action: 'clear' }) : new Response('ok')
        }
        const question = card.request.questions[card.index]!
        if (!modal && (choice.choice === 'other' || choice.choice === 'edit')) {
          const trigger = stringValue(payload.trigger_id)
          if (!trigger || (choice.choice === 'other' && question.allowOther === false)) return unavailable(false)
          const result = await slackCall(options, 'views.open', {
            trigger_id: trigger, view: questionModal(card, choice)
          })
          assertSlackOk('views.open', result)
          return new Response('ok')
        }
        const answer = modal
          ? modalAnswer(question, view!)
          : typeof choice.choice === 'number' && !question.multiSelect
            ? [question.options[choice.choice]?.label].filter((value): value is string => !!value)
            : []
        if (answer.length === 0) return modalError('Choose an option or enter an answer.', view)
        // Save before posting or updating Slack. Repeated final submissions send
        // the same request id/answers to the API's idempotent answer endpoint.
        const prior = Object.hasOwn(card.answers, question.id) ? card.answers[question.id] : undefined
        if (prior && JSON.stringify(prior.answers) !== JSON.stringify(answer)) return unavailable(modal, view)
        card.answers = { ...card.answers, [question.id]: { answers: answer } }
        const complete = card.index === card.request.questions.length - 1
        if (!complete) card.index += 1
        await saveCard(card, options)
        if (complete) {
          await submitSessionUserInput(options, card.threadId, card.request, card.answers, user)
          card.closed = true
          card.request.status = 'submitted'
          await saveCard(card, options)
        }
        await refresh(card)
        return modal ? Response.json({ response_action: 'clear' }) : new Response('ok')
      })
    } catch (error) {
      // Keep a modal open on transient failure. The saved answers make a repeat
      // submission safe even if the API accepted the first one before a timeout.
      options.logger?.warn('slackbotv2_user_input_failed', { request_id: choice.request, error: errorMessage(error) })
      return modal ? modalError('Could not send your answer. Please try again.', view)
        : new Response('Could not save the answer. Please click again.', { status: 503 })
    }
  }
  // Acknowledgement has Slack's deadline even when the durable handoff is
  // slow. Keep an in-flight handoff alive; retrying its immutable answers is
  // safe if it commits after the retryable response has been returned.
  const pending = handoff()
  keepAlive?.(pending)
  let timer: ReturnType<typeof setTimeout> | undefined
  try {
    return await Promise.race([pending, new Promise<Response>(resolve => {
      timer = setTimeout(() => resolve(modal
        ? modalError('Your answer is still being sent. Please try again.', view)
        : new Response('Answer handoff is still pending. Please click again.', { status: 503 })), INTERACTION_ACK_TIMEOUT_MS)
    })])
  } finally {
    clearTimeout(timer)
  }
}

function validateRequest(request: SessionUserInputRequest): void {
  if (!request.requester_user_id || !Array.isArray(request.questions)
    || request.questions.length < 1 || request.questions.length > 4
    || request.questions.some(question => !question.id || !question.question
      || !Array.isArray(question.options) || question.options.length > 10)) {
    throw new Error('Invalid native question request')
  }
}

function parseChoice(value: unknown): Choice {
  if (typeof value !== 'string') throw new Error('Missing choice')
  const item: unknown = JSON.parse(value)
  if (!isJsonObject(item) || typeof item.request !== 'string'
    || !/^[0-9a-f-]{36}$/.test(item.request) || !Number.isInteger(item.index)
    || typeof item.index !== 'number' || item.index < 0 || item.index > 3
    || !(item.choice === 'other' || item.choice === 'edit'
      || (typeof item.choice === 'number' && Number.isInteger(item.choice) && item.choice >= 0 && item.choice < 10))) {
    throw new Error('Invalid choice')
  }
  return item as Choice
}

function questionSections(question: SessionUserInputQuestion): JsonObject[] {
  const content = [question.question, ...question.options.map((option, index) =>
    `${index + 1}. ${option.label}${option.description ? `\n${option.description}` : ''}`)].join('\n\n')
  const blocks: JsonObject[] = []
  for (let start = 0; start < content.length; start += 2800) {
    blocks.push({ type: 'section', text: { type: 'plain_text', text: content.slice(start, start + 2800) } })
  }
  return blocks
}

function cardMessage(card: QuestionCard): JsonObject {
  const marker = `${PREFIX}${card.request.request_id}`
  if (card.closed) {
    const text = card.request.status === 'cancelled'
      ? 'This question is no longer waiting for an answer.'
      : card.request.status === 'submitted'
        ? 'Answers sent. Waiting for the task to continue.' : 'Answers received. Continuing the task.'
    return { text, blocks: [{ type: 'section', block_id: marker, text: { type: 'plain_text', text } }] }
  }
  const question = card.request.questions[card.index]!
  const title = `Question ${card.index + 1} of ${card.request.questions.length}${question.header ? ` · ${question.header}` : ''}`
  const button = (label: string, choice: Choice['choice']): JsonObject => ({
    type: 'button', text: { type: 'plain_text', text: label.slice(0, 75) },
    action_id: `${PREFIX}${choice}`,
    value: JSON.stringify({ request: card.request.request_id, index: card.index, choice })
  })
  const buttons = question.multiSelect
    ? [button('Choose answers', 'edit')]
    : question.options.map((option, index) => button(`${index + 1}. ${option.label}`, index))
  if (!question.multiSelect && question.allowOther !== false) buttons.push(button('Write an answer', 'other'))
  return {
    mrkdwn: false, unfurl_links: false, unfurl_media: false,
    text: `${title}: ${question.question}`,
    blocks: [
      { type: 'section', block_id: marker, text: { type: 'plain_text', text: title.slice(0, 3000) } },
      ...questionSections(question),
      { type: 'actions', elements: buttons },
      { type: 'context', elements: [{ type: 'plain_text', text: 'The person who requested this task can answer.' }] }
    ]
  }
}

function questionModal(card: QuestionCard, choice: Choice): JsonObject {
  const question = card.request.questions[card.index]!
  const blocks = questionSections(question)
  if (question.multiSelect && question.options.length) {
    blocks.push({ type: 'input', block_id: 'choices', optional: question.allowOther !== false,
      label: { type: 'plain_text', text: 'Choose all that apply' },
      element: { type: 'checkboxes', action_id: 'selected', options: question.options.map((option, index) => ({
        text: { type: 'plain_text', text: `${index + 1}. ${option.label}`.slice(0, 75) }, value: String(index)
      })) }
    })
  }
  if (question.allowOther !== false) {
    blocks.push({ type: 'input', block_id: 'custom', optional: question.multiSelect === true,
      label: { type: 'plain_text', text: 'Your answer' },
      element: { type: 'plain_text_input', action_id: 'text', multiline: true, max_length: 2000 }
    })
  }
  return {
    type: 'modal', callback_id: MODAL_ID,
    private_metadata: JSON.stringify({ ...choice, channel_id: card.channel }),
    title: { type: 'plain_text', text: `Question ${card.index + 1} of ${card.request.questions.length}` },
    submit: { type: 'plain_text', text: card.index === card.request.questions.length - 1 ? 'Send answers' : 'Next' },
    close: { type: 'plain_text', text: 'Cancel' }, blocks
  }
}

function modalAnswer(question: SessionUserInputQuestion, view: JsonObject): string[] {
  const state = isJsonObject(view.state) ? view.state : {}
  const values = isJsonObject(state.values) ? state.values : {}
  const choices = isJsonObject(values.choices) ? values.choices : {}
  const selected = isJsonObject(choices.selected) ? choices.selected.selected_options : []
  const answer = question.multiSelect && Array.isArray(selected)
    ? selected.flatMap(item => {
      const value = isJsonObject(item) ? item.value : undefined
      if (typeof value !== 'string' || !/^[0-9]$/.test(value)) return []
      const label = question.options[Number(value)]?.label
      return label ? [label] : []
    }) : []
  const custom = isJsonObject(values.custom) ? values.custom : {}
  const text = isJsonObject(custom.text) ? custom.text.value : undefined
  if (question.allowOther !== false && typeof text === 'string' && text.trim() && text.length <= 2000) {
    answer.push(text.trim())
  }
  return [...new Set(answer)]
}

async function closeCard(id: string, options: SlackbotV2Options): Promise<void> {
  if (!options.state) return
  await withCardLock(options.state, id, async () => {
    const card = await options.state!.get<QuestionCard>(cardKey(id))
    if (!card) return
    card.request = await getSessionUserInput(options, card.threadId, id)
    if (card.request.status === 'pending') return
    card.closed = true
    await saveCard(card, options)
    await updateCard(card, options)
  })
}

async function retryCardUpdate(id: string, operation: () => Promise<void>, options: SlackbotV2Options): Promise<void> {
  for (let attempt = 0; ; attempt++) {
    try { return await operation() } catch (error) {
      if (attempt === 2) {
        options.logger?.warn('slackbotv2_user_input_update_failed', {
          request_id: id, error: errorMessage(error)
        })
        return
      }
      await new Promise(resolve => setTimeout(resolve, 100 * (attempt + 1)))
    }
  }
}

async function updateCard(card: QuestionCard, options: SlackbotV2Options): Promise<void> {
  if (!card.messageTs) return
  const result = await slackCall(options, 'chat.update', {
    channel: card.channel, ts: card.messageTs, ...cardMessage(card)
  })
  assertSlackOk('chat.update', result)
}

async function findPostedCard(card: QuestionCard, options: SlackbotV2Options): Promise<string | undefined> {
  const botUserId = options.botUserId ?? stringValue((await slackCall(options, 'auth.test', {})).user_id)
  if (!botUserId) throw new Error('Cannot establish question card author')
  let cursor: string | undefined
  for (let page = 0; page < 10; page++) {
    const result = await withSlackApiTimeout(options, 'recover question card', () => fetchSlackThreadReplies({
      channel: card.channel, ts: card.threadTs, oldest: String((card.postingAt! - 5000) / 1000),
      limit: 100, cursor, apiUrl: options.slackApiUrl, token: options.botToken,
      fetch: options.fetch as typeof globalThis.fetch | undefined
    }))
    for (const message of result.messages) {
      if (!isJsonObject(message) || message.user !== botUserId
        || typeof message.ts !== 'string' || !Array.isArray(message.blocks)) continue
      if (message.blocks.some(block => isJsonObject(block)
        && block.block_id === `${PREFIX}${card.request.request_id}`)) return message.ts
    }
    cursor = result.nextCursor
    if (!cursor) return undefined
  }
  throw new Error('Question card recovery exceeded the thread page limit')
}

async function slackCall(options: SlackbotV2Options, method: string, body: JsonObject) {
  return withSlackApiTimeout(options, method, () => callSlackApi(method, body, {
    apiUrl: options.slackApiUrl, token: options.botToken,
    fetch: options.fetch as typeof globalThis.fetch | undefined
  }))
}

async function saveCard(card: QuestionCard, options: SlackbotV2Options): Promise<void> {
  await options.state!.set(cardKey(card.request.request_id), card, STATE_TTL_MS)
}

async function withCardLock<T>(state: StateAdapter, id: string, operation: () => Promise<T>): Promise<T> {
  let lock = await state.acquireLock(`${cardKey(id)}:lock`, LOCK_TTL_MS)
  // A resolved event can arrive while its answer webhook still updates the
  // card. Let that short handoff finish before replaying its presentation.
  for (let attempt = 0; !lock && attempt < 20; attempt++) {
    await new Promise(resolve => setTimeout(resolve, 50))
    lock = await state.acquireLock(`${cardKey(id)}:lock`, LOCK_TTL_MS)
  }
  if (!lock) throw new Error('Question is being updated; retry the interaction')
  try { return await operation() } finally { await state.releaseLock(lock) }
}

function modalError(text: string, view?: JsonObject): Response {
  const blocks = Array.isArray(view?.blocks) ? view.blocks : []
  const input = blocks.find(block => isJsonObject(block) && block.type === 'input')
  const blockId = isJsonObject(input) && typeof input.block_id === 'string' ? input.block_id : 'custom'
  return Response.json({ response_action: 'errors', errors: { [blockId]: text } })
}

function unavailable(modal: boolean, view?: JsonObject): Response {
  return modal ? modalError('This question is no longer available to you.', view) : new Response('ok')
}

import { createHash, createHmac, timingSafeEqual } from 'node:crypto'
import type { Hono } from 'hono'
import type { StateAdapter } from 'chat'
import type { SlackbotV2Options } from './types'
import {
  COMMAND_FORM,
  COMMAND_PICKER,
  COMMAND_RETRY,
  COMMAND_SEARCH,
  CommandFieldError,
  SlackCommandRegistry,
  commandView,
  formValues,
  pickerView,
  plain,
  type SlackModalView
} from './slack-command-registry'

const MODAL_TTL_MS = 60 * 60 * 1000
type Origin = {
  team: string
  channel: string
  user: string
  requestId: string
  expires: number
  command?: string
}
type PreparedCommand = {
  workflow: string
  input: Record<string, unknown>
  accepted: boolean
}

async function before<T>(
  deadline: number,
  operation: () => Promise<T>
): Promise<T> {
  const remaining = deadline - Date.now()
  if (remaining <= 0) throw new Error('Slack response deadline exceeded')
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error('Slack response deadline exceeded')),
      remaining
    )
    operation()
      .then(resolve, reject)
      .finally(() => clearTimeout(timer))
  })
}

export function verifySlackSignature(
  body: string,
  timestamp: string | undefined,
  signature: string | undefined,
  secret: string,
  now = Date.now()
): boolean {
  if (
    !timestamp ||
    !/^\d+$/.test(timestamp) ||
    Math.abs(now / 1000 - Number(timestamp)) > 300 ||
    !signature ||
    !/^v0=[a-f0-9]{64}$/.test(signature)
  )
    return false
  const expected =
    'v0=' +
    createHmac('sha256', secret).update(`v0:${timestamp}:${body}`).digest('hex')
  return timingSafeEqual(Buffer.from(signature), Buffer.from(expected))
}

function seal(origin: Origin, secret: string): string {
  const data = Buffer.from(JSON.stringify(origin)).toString('base64url')
  return `${data}.${createHmac('sha256', secret).update(`command-modal:${data}`).digest('hex')}`
}

function unseal(value: unknown, secret: string): Origin | undefined {
  if (typeof value !== 'string' || value.length > 3000) return
  const [data, signature, extra] = value.split('.')
  if (!data || !signature || extra || !/^[a-f0-9]{64}$/.test(signature)) return
  const expected = createHmac('sha256', secret)
    .update(`command-modal:${data}`)
    .digest('hex')
  if (!timingSafeEqual(Buffer.from(signature), Buffer.from(expected))) return
  try {
    const origin = JSON.parse(
      Buffer.from(data, 'base64url').toString()
    ) as Origin
    if (
      !origin ||
      !Number.isFinite(origin.expires) ||
      origin.expires <= Date.now() ||
      typeof origin.requestId !== 'string' ||
      !/^[a-f0-9]{64}$/.test(origin.requestId)
    )
      return
    return origin
  } catch {
    return
  }
}

function notice(
  title: string,
  text: string,
  metadata?: string
): SlackModalView {
  return {
    type: 'modal',
    title: plain(title),
    close: plain('Close'),
    callback_id: metadata ? COMMAND_RETRY : 'commands:receipt',
    private_metadata: metadata,
    submit: metadata ? plain('Retry') : undefined,
    blocks: [{ type: 'section', text: plain(text) }]
  }
}

/** All entry points resolve an explicit server-owned command; user input never chooses a workflow. */
export function mountSlashCommands(
  app: Hono,
  options: SlackbotV2Options,
  state: StateAdapter
): void {
  const config = options.slashCommands
  if (!config) return
  const registry = new SlackCommandRegistry(config.definitions)
  const fetchFn = options.fetch ?? fetch
  const allowed = (team: unknown, channel: unknown, user: unknown) =>
    typeof team === 'string' &&
    /^T[A-Z0-9]+$/.test(team) &&
    (!config.teamId || config.teamId === team) &&
    typeof channel === 'string' &&
    /^[CG][A-Z0-9]+$/.test(channel) &&
    typeof user === 'string' &&
    /^[UW][A-Z0-9]+$/.test(user)
  const enqueue = async (
    prepared: PreparedCommand,
    origin: Origin,
    deadline: number
  ) => {
    if (!options.apiKey || Date.now() >= deadline) return false
    const response = await fetchFn(
      new URL('/api/workflows/runs', options.apiUrl),
      {
        method: 'POST',
        headers: {
          authorization: `Bearer ${options.apiKey}`,
          'content-type': 'application/json'
        },
        body: JSON.stringify({
          workflow_name: prepared.workflow,
          eager_start: true,
          input: prepared.input,
          idempotency_key: `slack-command:${origin.requestId}`
        }),
        signal: AbortSignal.timeout(
          Math.max(1, Math.min(1800, deadline - Date.now()))
        )
      }
    ).catch(() => null)
    return response?.ok === true
  }
  const withIdentity = (input: Record<string, unknown>, origin: Origin) => ({
    ...input,
    team_id: origin.team,
    channel_id: origin.channel,
    actor_id: origin.user,
    request_id: origin.requestId
  })
  const submit = async (
    origin: Origin,
    workflow: string,
    deadline: number,
    input?: () => Record<string, unknown>
  ) => {
    const stateKey = `command-submission:${origin.requestId}`
    await before(deadline, () => state.connect())
    const lockRequest = state.acquireLock(stateKey, 10_000)
    const lock = await before(deadline, () => lockRequest).catch((error) => {
      // A database operation can finish after our response budget; release a late lock.
      void lockRequest
        .then((value) => (value ? state.releaseLock(value) : undefined))
        .catch(() => {})
      throw error
    })
    if (!lock) return 'pending'
    try {
      let prepared = await before(deadline, () =>
        state.get<PreparedCommand>(stateKey)
      )
      if (!prepared) {
        if (!input) return 'missing'
        const candidate = {
          workflow,
          input: withIdentity(input(), origin),
          accepted: false
        }
        // Freeze relative deadlines and fields BEFORE handoff. The default state adapter is Postgres.
        const saved = await before(deadline, () =>
          state.setIfNotExists(stateKey, candidate, MODAL_TTL_MS * 2)
        )
        prepared = saved
          ? candidate
          : await before(deadline, () => state.get<PreparedCommand>(stateKey))
        if (!prepared) throw new Error('Submission state unavailable')
      }
      if (prepared.workflow !== workflow) return 'missing'
      if (!prepared.accepted) {
        if (
          !(await before(deadline, () => enqueue(prepared!, origin, deadline)))
        )
          return 'pending'
        prepared.accepted = true
        await before(deadline, () =>
          state.set(stateKey, prepared, MODAL_TTL_MS * 2)
        )
      }
      return 'accepted'
    } finally {
      const release = state.releaseLock(lock).catch(() => {})
      await before(deadline, () => release).catch(() => {})
    }
  }

  app.post('/api/slack/commands', async (c, next) => {
    const deadline = Date.now() + 2400
    const body = await c.req.raw.clone().text()
    const form = new URLSearchParams(body)
    if (form.get('command') !== config.name) return next()
    if (
      !verifySlackSignature(
        body,
        c.req.header('x-slack-request-timestamp'),
        c.req.header('x-slack-signature'),
        options.signingSecret
      )
    )
      return c.text('Unauthorized', 401)
    const team = form.get('team_id'),
      channel = form.get('channel_id'),
      user = form.get('user_id')
    if (!allowed(team, channel, user))
      return c.json({
        response_type: 'ephemeral',
        text: 'This command is not enabled in this channel.'
      })
    const text = (form.get('text') ?? '').trim()
    const name = text.split(/\s+/)[0] ?? ''
    const command = registry.get(name)
    if (text && !command)
      return c.json({
        response_type: 'ephemeral',
        text: `Unknown command. Run ${config.name} to search available commands.`
      })
    if (command && !command.enabled)
      return c.json({
        response_type: 'ephemeral',
        text: 'This command is currently disabled.'
      })
    const origin: Origin = {
      team: team!,
      channel: channel!,
      user: user!,
      requestId: createHash('sha256')
        .update(`${team}:${form.get('trigger_id')}:${body}`)
        .digest('hex'),
      expires: Date.now() + MODAL_TTL_MS,
      command: command?.name
    }

    if (!text || text === name) {
      if (!form.get('trigger_id')) return c.text('Missing trigger', 400)
      const metadata = seal(origin, options.signingSecret)
      const view = command
        ? commandView(command, metadata)
        : pickerView(config.name, metadata)
      try {
        const response = await fetchFn(
          new URL(
            'views.open',
            options.slackApiUrl ?? 'https://slack.com/api/'
          ),
          {
            method: 'POST',
            headers: {
              authorization: `Bearer ${options.botToken}`,
              'content-type': 'application/json'
            },
            body: JSON.stringify({
              trigger_id: form.get('trigger_id'),
              view: { ...view, external_id: origin.requestId }
            }),
            signal: AbortSignal.timeout(1800)
          }
        )
        const result = (await response.json()) as { ok?: boolean }
        if (!response.ok || !result.ok) throw new Error('Modal open failed')
        return c.text('')
      } catch {
        return c.json({
          response_type: 'ephemeral',
          text: `Could not open the command form. Please run ${config.name} again.`
        })
      }
    }
    try {
      const result = await submit(
        origin,
        command!.workflowName,
        deadline,
        () => {
          try {
            return command!.parseText(text, new Date())
          } catch (error) {
            throw new CommandFieldError('text', (error as Error).message)
          }
        }
      )
      if (result !== 'accepted')
        return c.text('Command handoff failed; please retry.', 503)
    } catch (error) {
      if (error instanceof CommandFieldError)
        return c.json({
          response_type: 'ephemeral',
          text: `${error.message} Nothing was created.\nExample: ${config.name} ${command!.example}`
        })
      return c.text('Command handoff failed; please retry.', 503)
    }
    return c.json({
      response_type: 'ephemeral',
      text: 'Request received. I’ll post the result in this channel.'
    })
  })

  // Slack calls this as a user types in the modal's external-select command search.
  app.post('/api/slack/options', async (c, next) => {
    const body = await c.req.raw.clone().text()
    let payload: any
    try {
      payload = JSON.parse(new URLSearchParams(body).get('payload') ?? '{}')
    } catch {
      return next()
    }
    if (payload?.action_id !== COMMAND_SEARCH) return next()
    if (
      !verifySlackSignature(
        body,
        c.req.header('x-slack-request-timestamp'),
        c.req.header('x-slack-signature'),
        options.signingSecret
      )
    )
      return c.text('Unauthorized', 401)
    const origin = unseal(payload.view?.private_metadata, options.signingSecret)
    if (
      payload.type !== 'block_suggestion' ||
      payload.view?.callback_id !== COMMAND_PICKER ||
      !origin ||
      origin.command ||
      payload.team?.id !== origin.team ||
      payload.user?.id !== origin.user ||
      !allowed(origin.team, origin.channel, origin.user)
    )
      return c.text('Forbidden', 403)
    return c.json({
      options: registry.search(
        typeof payload.value === 'string' ? payload.value.slice(0, 200) : ''
      )
    })
  })

  app.post('/api/slack/actions', async (c, next) => {
    const deadline = Date.now() + 2400
    const body = await c.req.raw.clone().text()
    let payload: any
    try {
      payload = JSON.parse(new URLSearchParams(body).get('payload') ?? '{}')
    } catch {
      return next()
    }
    const callback = payload?.view?.callback_id
    if (![COMMAND_PICKER, COMMAND_FORM, COMMAND_RETRY].includes(callback))
      return next()
    if (
      !verifySlackSignature(
        body,
        c.req.header('x-slack-request-timestamp'),
        c.req.header('x-slack-signature'),
        options.signingSecret
      )
    )
      return c.text('Unauthorized', 401)
    const origin = unseal(payload.view?.private_metadata, options.signingSecret)
    if (
      !origin ||
      payload.team?.id !== origin.team ||
      payload.user?.id !== origin.user ||
      !allowed(origin.team, origin.channel, origin.user)
    )
      return c.json({
        response_action: 'update',
        view: notice(
          'Form unavailable',
          `This form expired or belongs to another user. Run ${config.name} again.`
        )
      })
    if (payload.type !== 'view_submission') return c.text('')
    if (callback === COMMAND_PICKER) {
      if (origin.command) return c.text('Invalid picker', 400)
      const name =
        payload.view.state?.values?.command?.[COMMAND_SEARCH]?.selected_option
          ?.value
      const command = typeof name === 'string' ? registry.get(name) : undefined
      if (!command?.enabled)
        return c.json({
          response_action: 'errors',
          errors: { command: 'Choose an available command.' }
        })
      return c.json({
        response_action: 'update',
        view: commandView(
          command,
          seal(
            {
              ...origin,
              command: command.name,
              requestId: createHash('sha256')
                .update(`${origin.requestId}:${command.name}`)
                .digest('hex')
            },
            options.signingSecret
          )
        )
      })
    }
    const command = origin.command ? registry.get(origin.command) : undefined
    if (!command?.enabled)
      return c.json({
        response_action: 'update',
        view: notice(
          'Command unavailable',
          `This command is disabled. Run ${config.name} to choose another.`
        )
      })
    const metadata = seal(origin, options.signingSecret)
    const retry = () =>
      c.json({
        response_action: 'update',
        view: notice(
          'Delivery not confirmed',
          'This request may already have been received. Retry safely sends the same details without creating a duplicate. Closing this window does not cancel a received request.',
          metadata
        )
      })
    try {
      const result = await submit(
        origin,
        command.workflowName,
        deadline,
        callback === COMMAND_RETRY
          ? undefined
          : () =>
              command.parseForm(
                formValues(command, payload.view.state?.values),
                new Date()
              )
      )
      if (result === 'missing')
        return c.json({
          response_action: 'update',
          view: notice(
            'Request unavailable',
            `No matching saved submission was found. Run ${config.name} again.`
          )
        })
      if (result !== 'accepted') return retry()
      return c.json({
        response_action: 'update',
        view: notice(
          'Request received',
          'Your request was received. The result or any account validation errors will appear in the channel where you opened this form.'
        )
      })
    } catch (error) {
      if (error instanceof CommandFieldError)
        return c.json({
          response_action: 'errors',
          errors: { [error.field]: error.message }
        })
      return retry()
    }
  })
}

import {
  CommandFieldError,
  type CommandField,
  type SlackCommandDefinition
} from './slack-command-registry'

const USER = /^<@([UW][A-Z0-9]+)(?:\|[^>]+)?>$/
const KEY = /^(INC|CHR)-[1-9][0-9]*$/

export function durationSeconds(value: string): number {
  const match = /^([1-9][0-9]*)(m|h|d)$/.exec(value)
  if (!match) throw new Error('Use a duration such as 5m, 2h, or 1d.')
  const seconds =
    Number(match[1]) * ({ m: 60, h: 3600, d: 86400 }[match[2]!] ?? 0)
  if (!Number.isSafeInteger(seconds) || seconds > 366 * 86400)
    throw new Error('Duration must be at most 366 days.')
  return seconds
}

export function parseWorkItemCommand(
  text: string,
  now = new Date()
): Record<string, unknown> {
  const tokens = text.match(/"[^"\n]*"|\S+/g) ?? []
  const operation = tokens.shift()
  if (operation === 'incident' || operation === 'chore') {
    const title = tokens.shift()
    if (
      !title?.startsWith('"') ||
      !title.endsWith('"') ||
      !title.slice(1, -1).trim() ||
      title.length > 502
    ) {
      throw new Error(
        'Provide a nonempty quoted description (at most 500 characters).'
      )
    }
    const fields: Record<string, string> = {}
    for (const token of tokens) {
      const colon = token.indexOf(':')
      const key = token.slice(0, colon)
      if (colon < 1 || !['owner', 'deadline'].includes(key) || fields[key])
        throw new Error('Use exactly one owner and one deadline.')
      fields[key] = token.slice(colon + 1)
    }
    const owner = USER.exec(fields.owner ?? '')?.[1]
    if (!owner)
      throw new Error('Missing or invalid owner. Select a Slack user mention.')
    if (!fields.deadline)
      throw new Error('Missing required argument: deadline.')
    let deadline: Date
    if (/^[1-9][0-9]*[mhd]$/.test(fields.deadline)) {
      deadline = new Date(
        now.getTime() + durationSeconds(fields.deadline) * 1000
      )
    } else {
      if (
        !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2})?(?:Z|[+-]\d{2}:\d{2})$/.test(
          fields.deadline
        )
      ) {
        throw new Error(
          'Use a duration or an ISO timestamp with an explicit timezone.'
        )
      }
      const [year, month, day] = fields.deadline
        .slice(0, 10)
        .split('-')
        .map(Number)
      if (
        !year ||
        !month ||
        !day ||
        month > 12 ||
        day > new Date(Date.UTC(year, month, 0)).getUTCDate()
      )
        throw new Error('Deadline contains an invalid calendar date.')
      deadline = new Date(fields.deadline)
    }
    if (
      !Number.isFinite(deadline.getTime()) ||
      deadline <= now ||
      deadline.getTime() - now.getTime() > 366 * 86400_000
    )
      throw new Error('Deadline must be in the future.')
    return {
      operation: 'create',
      kind: operation,
      title: title.slice(1, -1).trim(),
      owner_id: owner,
      created_at: now.toISOString(),
      deadline_at: deadline.toISOString()
    }
  }
  if (
    !['acknowledge', 'resolve', 'snooze', 'status', 'reschedule'].includes(
      operation ?? ''
    )
  )
    throw new Error(
      'Choose incident, chore, acknowledge, resolve, snooze, status, or reschedule.'
    )
  const key = tokens.shift()
  if (!key || !KEY.test(key))
    throw new Error('Provide an item ID, such as INC-42 or CHR-18.')
  const result: Record<string, unknown> = { operation, key }
  if (operation === 'snooze' || operation === 'reschedule')
    result.duration_seconds = durationSeconds(tokens.shift() ?? '')
  if (tokens.length) throw new Error('Unexpected extra arguments.')
  return result
}

const itemField: CommandField = {
  id: 'key',
  label: 'Item ID',
  type: 'text',
  placeholder: 'INC-42 or CHR-18',
  maxLength: 30
}
const deadlineField: CommandField = {
  id: 'deadline',
  label: 'Deadline',
  type: 'text',
  maxLength: 40,
  placeholder: '30m, 2h, or 2026-12-01T17:00:00Z',
  hint: 'Durations start when you submit. For a date and time, include the timezone.'
}

/** Register new domains alongside these entries; the shared router owns no work-item branches. */
export function workItemCommands(
  creationEnabled: boolean
): SlackCommandDefinition[] {
  const create = (kind: 'incident' | 'chore'): SlackCommandDefinition => ({
    name: kind,
    title: kind === 'incident' ? 'Create incident' : 'Create chore',
    description:
      kind === 'incident'
        ? 'Track an incident with an owner, deadline, and issue.'
        : 'Assign a chore with a deadline and reminders.',
    keywords:
      kind === 'incident'
        ? ['create', 'outage', 'issue']
        : ['create', 'task', 'todo'],
    enabled: creationEnabled,
    workflowName: 'work_item_command',
    submitLabel: 'Create',
    example: `${kind} "Investigate a failing job" owner:@alex deadline:30m`,
    fields: [
      { id: 'title', label: 'Description', type: 'text', maxLength: 500 },
      { id: 'owner', label: 'Owner', type: 'user' },
      deadlineField
    ],
    parseText: parseWorkItemCommand,
    parseForm(values, now) {
      if (!/^[UW][A-Z0-9]+$/.test(values.owner ?? ''))
        throw new CommandFieldError('owner', 'Choose a Slack workspace member.')
      try {
        // Reuse the text command's deadline validation; the actual title is a structured field.
        const parsed = parseWorkItemCommand(
          `${kind} "Form" owner:<@${values.owner}> deadline:${values.deadline}`,
          now
        )
        return { ...parsed, title: values.title }
      } catch (error) {
        throw new CommandFieldError('deadline', (error as Error).message)
      }
    }
  })
  const actions = [
    [
      'acknowledge',
      'Acknowledge item',
      'Accept an item or stop repeated overdue reminders.',
      'Acknowledge',
      ['ack', 'accept']
    ],
    [
      'resolve',
      'Resolve item',
      'Mark an incident or chore complete and stop its reminders.',
      'Resolve',
      ['done', 'complete', 'close']
    ],
    [
      'snooze',
      'Snooze reminders',
      'Temporarily pause overdue reminders without moving the deadline.',
      'Snooze',
      ['pause', 'remind']
    ],
    [
      'reschedule',
      'Reschedule item',
      'Set a new deadline and restart the warning schedule.',
      'Reschedule',
      ['extend', 'deadline']
    ],
    [
      'status',
      'Check item status',
      'Show the current status of an incident or chore in this channel.',
      'Check status',
      ['progress', 'show']
    ]
  ] as const
  return [
    create('incident'),
    create('chore'),
    ...actions.map(
      ([
        name,
        title,
        description,
        submitLabel,
        keywords
      ]): SlackCommandDefinition => ({
        name,
        title,
        description,
        submitLabel,
        keywords,
        enabled: true,
        workflowName: 'work_item_command',
        example: `${name} INC-42${name === 'snooze' || name === 'reschedule' ? ' 15m' : ''}`,
        fields: [
          itemField,
          ...(name === 'snooze' || name === 'reschedule'
            ? [
                {
                  id: 'duration',
                  label:
                    name === 'snooze'
                      ? 'Pause reminders for'
                      : 'New deadline from now',
                  type: 'text' as const,
                  placeholder: '15m or 2h',
                  maxLength: 12,
                  hint:
                    name === 'snooze'
                      ? 'Between 1 minute and 1 day. Available for overdue, unacknowledged items.'
                      : 'Up to 366 days.'
                }
              ]
            : [])
        ],
        parseText: parseWorkItemCommand,
        parseForm(values, now) {
          if (!KEY.test(values.key ?? ''))
            throw new CommandFieldError(
              'key',
              'Use an item ID such as INC-42 or CHR-18.'
            )
          try {
            const parsed = parseWorkItemCommand(
              `${name} ${values.key}${values.duration ? ` ${values.duration}` : ''}`,
              now
            )
            if (name === 'snooze' && Number(parsed.duration_seconds) > 86400)
              throw new Error('Snooze must be between 1 minute and 1 day.')
            return parsed
          } catch (error) {
            throw new CommandFieldError('duration', (error as Error).message)
          }
        }
      })
    )
  ]
}

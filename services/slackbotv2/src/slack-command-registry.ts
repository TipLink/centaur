import type { ViewsOpenArguments } from '@slack/web-api'

export type SlackModalView = ViewsOpenArguments['view']

/** Server-owned catalog: a command supplies its UI and parser, never a client workflow name. */
export type CommandField = {
  id: string
  label: string
  type: 'text' | 'user'
  hint?: string
  placeholder?: string
  maxLength?: number
}

export type SlackCommandDefinition = {
  name: string
  title: string
  description: string
  keywords: readonly string[]
  enabled: boolean
  workflowName: string
  example: string
  submitLabel: string
  fields: readonly CommandField[]
  parseText: (text: string, now: Date) => Record<string, unknown>
  parseForm: (
    values: Record<string, string>,
    now: Date
  ) => Record<string, unknown>
}

export type SlashCommandConfig = {
  name: string
  teamId?: string
  definitions: readonly SlackCommandDefinition[]
}

export class CommandFieldError extends Error {
  constructor(
    readonly field: string,
    message: string
  ) {
    super(message)
  }
}

export const plain = (text: string) => ({ type: 'plain_text' as const, text })
export const COMMAND_PICKER = 'commands:picker'
export const COMMAND_FORM = 'commands:form'
export const COMMAND_RETRY = 'commands:retry'
export const COMMAND_SEARCH = 'commands:search'

export class SlackCommandRegistry {
  private readonly definitions = new Map<string, SlackCommandDefinition>()

  constructor(commands: readonly SlackCommandDefinition[]) {
    for (const command of commands) {
      if (
        !/^[a-z][a-z0-9-]*$/.test(command.name) ||
        this.definitions.has(command.name)
      )
        throw new Error(`Invalid or duplicate command: ${command.name}`)
      if (
        !command.fields.length ||
        command.fields.length > 90 ||
        command.title.length > 24 ||
        command.submitLabel.length > 24 ||
        command.description.length > 75 ||
        !command.workflowName
      )
        throw new Error(`Invalid command definition: ${command.name}`)
      const fields = new Set<string>()
      for (const field of command.fields) {
        if (!/^[a-z][a-z0-9_-]*$/.test(field.id) || fields.has(field.id))
          throw new Error(
            `Invalid or duplicate field: ${command.name}.${field.id}`
          )
        fields.add(field.id)
      }
      this.definitions.set(command.name, command)
    }
  }

  get(name: string): SlackCommandDefinition | undefined {
    return this.definitions.get(name)
  }

  search(query: string) {
    const words = query.trim().toLowerCase().split(/\s+/)
    return [...this.definitions.values()]
      .filter(
        (command) =>
          command.enabled &&
          words.every((word) =>
            [
              command.name,
              command.title,
              command.description,
              ...command.keywords
            ]
              .join(' ')
              .toLowerCase()
              .includes(word)
          )
      )
      .sort((a, b) => Number(b.name === query) - Number(a.name === query))
      .slice(0, 100)
      .map((command) => ({
        text: plain(command.title),
        value: command.name,
        description: plain(command.description)
      }))
  }
}

export function pickerView(name: string, metadata: string): SlackModalView {
  return {
    type: 'modal',
    callback_id: COMMAND_PICKER,
    private_metadata: metadata,
    title: plain(`${name} commands`.slice(0, 24)),
    submit: plain('Next'),
    close: plain('Cancel'),
    blocks: [
      {
        type: 'section',
        text: plain(
          'Search for a command, then choose Next to fill in its details.'
        )
      },
      {
        type: 'input',
        block_id: 'command',
        label: plain('Command'),
        element: {
          type: 'external_select',
          action_id: COMMAND_SEARCH,
          min_query_length: 0,
          placeholder: plain('Search commands…')
        }
      }
    ]
  }
}

export function commandView(
  command: SlackCommandDefinition,
  metadata: string
): SlackModalView {
  return {
    type: 'modal',
    callback_id: COMMAND_FORM,
    private_metadata: metadata,
    title: plain(command.title),
    submit: plain(command.submitLabel),
    close: plain('Cancel'),
    blocks: [
      { type: 'section', text: plain(command.description) },
      ...command.fields.map((field) => ({
        type: 'input' as const,
        block_id: field.id,
        label: plain(field.label),
        hint: field.hint ? plain(field.hint) : undefined,
        element:
          field.type === 'user'
            ? {
                type: 'users_select' as const,
                action_id: 'value',
                placeholder: plain('Choose an owner')
              }
            : {
                type: 'plain_text_input' as const,
                action_id: 'value',
                max_length: field.maxLength ?? 500,
                placeholder: field.placeholder
                  ? plain(field.placeholder)
                  : undefined
              }
      }))
    ]
  }
}

export function formValues(
  command: SlackCommandDefinition,
  state: any
): Record<string, string> {
  return Object.fromEntries(
    command.fields.map((field) => {
      const input = state?.[field.id]?.value
      const value = field.type === 'user' ? input?.selected_user : input?.value
      if (typeof value !== 'string' || !value.trim())
        throw new CommandFieldError(field.id, `${field.label} is required.`)
      if (field.type === 'text' && value.length > (field.maxLength ?? 500))
        throw new CommandFieldError(field.id, `${field.label} is too long.`)
      return [field.id, value.trim()]
    })
  )
}

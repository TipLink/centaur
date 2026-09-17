import { expect, test } from 'bun:test'
import {
  commandView,
  formValues,
  type SlackCommandDefinition
} from '../src/slack-command-registry'

const command: SlackCommandDefinition = {
  name: 'incident',
  title: 'Create incident',
  description: 'Track an incident.',
  keywords: [],
  enabled: true,
  workflowName: 'work_item_command',
  example: '',
  submitLabel: 'Create',
  fields: [
    { id: 'title', label: 'Title', type: 'text' },
    {
      id: 'description',
      label: 'Description',
      type: 'text',
      optional: true,
      multiline: true,
      maxLength: 2000
    },
    {
      id: 'owners',
      label: 'Additional owners',
      type: 'users',
      optional: true,
      maxSelectedItems: 9
    }
  ],
  parseText: () => ({}),
  parseForm: (values) => values
}

test('renders optional multiline text and multiple user selection', () => {
  const view = commandView(command, 'metadata')
  expect(view.blocks[2]).toMatchObject({
    type: 'input',
    optional: true,
    element: { type: 'plain_text_input', multiline: true, max_length: 2000 }
  })
  expect(view.blocks[3]).toMatchObject({
    type: 'input',
    optional: true,
    element: { type: 'multi_users_select', max_selected_items: 9 }
  })
  expect(
    formValues(command, { title: { value: { value: 'Investigate' } } })
  ).toEqual({ title: 'Investigate', description: '', owners: '' })
})

test('preserves multiline content and returns selected users through the extension contract', () => {
  expect(
    formValues(command, {
      title: { value: { value: 'Investigate' } },
      description: { value: { value: 'First line\nSecond line' } },
      owners: { value: { selected_users: ['U1', 'U2', 'U1'] } }
    })
  ).toEqual({
    title: 'Investigate',
    description: 'First line\nSecond line',
    owners: 'U1,U2'
  })
})

test('rejects missing required fields, excessive text, and invalid or excessive owners', () => {
  expect(() => formValues(command, {})).toThrow('Title is required')
  const title = { value: { value: 'Title' } }
  expect(() =>
    formValues(command, {
      title,
      description: { value: { value: 'x'.repeat(2001) } }
    })
  ).toThrow('too long')
  for (const selected_users of [
    'U1',
    ['invalid'],
    Array.from({ length: 10 }, (_, i) => `U${i}`)
  ]) {
    expect(() =>
      formValues(command, { title, owners: { value: { selected_users } } })
    ).toThrow()
  }
})

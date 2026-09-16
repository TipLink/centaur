import { afterEach, describe, expect, test } from 'bun:test'
import { mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { loadSlackCommandExtensions } from '../src/slack-command-extensions'

const directories: string[] = []
afterEach(async () => {
  await Promise.all(directories.splice(0).map((path) => rm(path, { recursive: true })))
})

async function moduleFile(source: string) {
  const directory = await mkdtemp(join(tmpdir(), 'slack-command-extension-'))
  directories.push(directory)
  const path = join(directory, 'extension.mjs')
  await writeFile(path, source)
  return path
}

describe('Slack command extension loading', () => {
  test('loads command factories and injects the shared field error type', async () => {
    const path = await moduleFile(`
      export default ({ CommandFieldError }) => ({
        commands: [{
          name: 'deploy', title: 'Deploy', description: 'Deploy a service',
          keywords: ['release'], enabled: true, workflowName: 'deploy_service',
          example: 'deploy staging', submitLabel: 'Deploy',
          fields: [{ id: 'target', label: 'Target', type: 'text' }],
          parseText: text => ({ target: text.split(' ')[1] }),
          parseForm: values => {
            if (!values.target) throw new CommandFieldError('target', 'Required')
            return values
          }
        }]
      })
    `)
    const [extension] = await loadSlackCommandExtensions([path])
    expect(extension!.commands[0]!.workflowName).toBe('deploy_service')
    expect(() => extension!.commands[0]!.parseForm({}, new Date())).toThrow(
      'Required'
    )
  })

  test('rejects modules without a factory or command array', async () => {
    const missingFactory = await moduleFile('export default { commands: [] }')
    await expect(loadSlackCommandExtensions([missingFactory])).rejects.toThrow(
      'factory function'
    )
    const missingCommands = await moduleFile('export default () => ({})')
    await expect(loadSlackCommandExtensions([missingCommands])).rejects.toThrow(
      'commands as an array'
    )
  })
})

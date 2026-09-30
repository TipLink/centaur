import { copyFile, mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'bun:test'
import type { ActionEvent, ActionHandler, Chat, Logger } from 'chat'
import {
  createSlackbotExtensionClaims,
  loadSlackbotExtensions,
  parseSlackbotExtensionModules,
  registerSlackbotExtension,
  type SlackbotExtensionContext
} from '../src/extensions'

const RESERVED_ACTION_PREFIX = 'centaur.workflow.action:'
const REVISION = '0123456789abcdef0123456789abcdef01234567'

describe('Slackbot extensions', () => {
  it('parses a JSON module list and rejects invalid configuration', () => {
    expect(parseSlackbotExtensionModules(undefined)).toEqual([])
    expect(parseSlackbotExtensionModules(JSON.stringify([{
      modulePath: './overlay/slack/index.mjs',
      repositoryPath: './overlay',
      revision: REVISION.toUpperCase()
    }]))).toEqual([{
      modulePath: resolve('./overlay/slack/index.mjs'),
      repositoryPath: resolve('./overlay'),
      revision: REVISION
    }])
    expect(() => parseSlackbotExtensionModules('one.mjs')).toThrow('must be a JSON array')
    expect(() => parseSlackbotExtensionModules('[""]')).toThrow('must be an object')
    expect(() => parseSlackbotExtensionModules(JSON.stringify([{
      modulePath: './overlay/slack/index.mjs',
      repositoryPath: './overlay',
      revision: 'main'
    }]))).toThrow('revision must be a full 40-character commit SHA')
    expect(() => parseSlackbotExtensionModules(JSON.stringify([{
      modulePath: './outside.mjs',
      repositoryPath: './overlay',
      revision: REVISION
    }]))).toThrow('modulePath must be inside repositoryPath')
    const duplicate = {
      modulePath: './overlay/slack/index.mjs',
      repositoryPath: './overlay',
      revision: REVISION
    }
    expect(() => parseSlackbotExtensionModules(JSON.stringify([
      duplicate,
      duplicate
    ]))).toThrow(
      'must not contain duplicate module paths'
    )
  })

  it('loads only the declared immutable repo-cache revision', async () => {
    const repositoryPath = await mkdtemp(`${tmpdir()}/centaur-slack-extension-`)
    const gitPath = resolve(repositoryPath, '.git')
    const modulePath = resolve(repositoryPath, 'valid.mjs')
    const invalidPath = resolve(repositoryPath, 'invalid.mjs')
    await mkdir(gitPath)
    await writeFile(resolve(gitPath, 'HEAD'), `${REVISION}\n`)
    await copyFile(
      fileURLToPath(new URL('./fixtures/extensions/valid.mjs', import.meta.url)),
      modulePath
    )
    await copyFile(
      fileURLToPath(new URL('./fixtures/extensions/invalid.mjs', import.meta.url)),
      invalidPath
    )
    const moduleConfig = { modulePath, repositoryPath, revision: REVISION }

    try {
      const registered: string[] = []
      const logs: string[] = []
      const context = {
        chat: {
          onSlashCommand(command: string | string[]) {
            registered.push(...(Array.isArray(command) ? command : [command]))
          }
        } as unknown as Chat,
        logger: logger(logs)
      } as SlackbotExtensionContext
      const claims = createSlackbotExtensionClaims(RESERVED_ACTION_PREFIX)
      await loadSlackbotExtensions([moduleConfig], context, claims)

      expect(registered).toEqual(['/fixture'])
      expect(logs).toContain('fixture_extension_registered')
      expect(logs).toContain('slackbotv2_extension_loaded')

      await expect(loadSlackbotExtensions([
        { ...moduleConfig, modulePath: invalidPath }
      ], context, claims)).rejects.toThrow('must export an extension manifest')
      await expect(loadSlackbotExtensions([
        { ...moduleConfig, revision: 'ffffffffffffffffffffffffffffffffffffffff' }
      ], context, claims)).rejects.toThrow(`requires revision ${'f'.repeat(40)}, found ${REVISION}`)
    } finally {
      await rm(repositoryPath, { recursive: true, force: true })
    }
  })

  it('claims exact and prefixed actions without taking Centaur workflow actions', () => {
    const claims = createSlackbotExtensionClaims(RESERVED_ACTION_PREFIX)
    claims.claim({
      id: 'work-items',
      actionIds: ['work_item.resolve'],
      actionIdPrefixes: ['work_item.legacy:'],
      modalCallbackIds: ['work_item.create'],
      optionsLoadIds: ['work_item.owner'],
      slashCommands: ['/work-item']
    }, '/overlay/work-items/index.mjs')

    expect(claims.ownsAction('work_item.resolve')).toBe(true)
    expect(claims.ownsAction('work_item.owner')).toBe(true)
    expect(claims.ownsAction('work_item.legacy:INC-42')).toBe(true)
    expect(claims.ownsModalCallback('work_item.create')).toBe(true)
    expect(claims.ownsOptionsLoad('work_item.owner')).toBe(true)
    expect(claims.ownsAction('deploy.approve')).toBe(false)

    expect(() => claims.claim({
      id: 'duplicate',
      actionIds: ['work_item.resolve']
    }, '/overlay/duplicate/index.mjs')).toThrow('already claimed by work-items')
    expect(() => claims.claim({
      id: 'reserved',
      actionIds: ['centaur.workflow.action:run:approve']
    }, '/overlay/reserved/index.mjs')).toThrow('reserved by Centaur')
    expect(() => claims.claim({
      id: 'reserved-prefix',
      actionIdPrefixes: ['centaur.workflow.']
    }, '/overlay/reserved-prefix/index.mjs')).toThrow("overlaps Centaur's reserved prefix")
    expect(() => claims.claim({
      id: 'self-overlap',
      actionIds: ['self.resolve'],
      actionIdPrefixes: ['self.']
    }, '/overlay/self-overlap/index.mjs')).toThrow('overlaps an ID claimed by self-overlap')
    expect(() => claims.claim({
      id: 'nested-prefixes',
      actionIdPrefixes: ['nested.', 'nested.specific.']
    }, '/overlay/nested-prefixes/index.mjs')).toThrow('overlaps a prefix claimed by nested-prefixes')
  })

  it('rejects undeclared registrations and declarations without handlers', async () => {
    const chat = {
      onSlashCommand() {}
    } as unknown as Chat
    const context = { chat, logger: logger([]) } as SlackbotExtensionContext

    await expectRegistrationError(registerSlackbotExtension(
      { id: 'undeclared', slashCommands: ['/fineas'] },
      ({ chat: scopedChat }) => {
        scopedChat.onSlashCommand('/other', async () => {})
      },
      context,
      createSlackbotExtensionClaims(RESERVED_ACTION_PREFIX),
      '/overlay/undeclared/index.mjs'
    ), 'is not declared in its manifest')

    await expectRegistrationError(registerSlackbotExtension(
      { id: 'missing', slashCommands: ['/fineas'] },
      async () => {},
      context,
      createSlackbotExtensionClaims(RESERVED_ACTION_PREFIX),
      '/overlay/missing/index.mjs'
    ), 'were declared but not registered: /fineas')

    await expectRegistrationError(registerSlackbotExtension(
      { id: 'whitespace', slashCommands: [' /fineas '] },
      ({ chat: scopedChat }) => {
        scopedChat.onSlashCommand(' /fineas ', async () => {})
      },
      context,
      createSlackbotExtensionClaims(RESERVED_ACTION_PREFIX),
      '/overlay/whitespace/index.mjs'
    ), 'must not contain surrounding whitespace')

    await expectRegistrationError(registerSlackbotExtension(
      { id: 'unsupported-registration' },
      ({ chat: scopedChat }) => {
        const unsafe = scopedChat as unknown as { onNewMention(handler: () => void): void }
        unsafe.onNewMention(() => {})
      },
      context,
      createSlackbotExtensionClaims(RESERVED_ACTION_PREFIX),
      '/overlay/unsupported-registration/index.mjs'
    ), 'registration method onNewMention')

    await expectRegistrationError(registerSlackbotExtension(
      { id: 'duplicate-handler', slashCommands: ['/fineas'] },
      ({ chat: scopedChat }) => {
        scopedChat.onSlashCommand('/fineas', async () => {})
        scopedChat.onSlashCommand('/fineas', async () => {})
      },
      context,
      createSlackbotExtensionClaims(RESERVED_ACTION_PREFIX),
      '/overlay/duplicate-handler/index.mjs'
    ), 'is registered more than once')

    const actionContext = {
      chat: { onAction() {} } as unknown as Chat,
      logger: logger([])
    } as SlackbotExtensionContext
    await expectRegistrationError(registerSlackbotExtension(
      { id: 'duplicate-prefix-handler', actionIdPrefixes: ['legacy:'] },
      ({ chat: scopedChat }) => {
        scopedChat.onAction(async () => {})
        scopedChat.onAction(async () => {})
      },
      actionContext,
      createSlackbotExtensionClaims(RESERVED_ACTION_PREFIX),
      '/overlay/duplicate-prefix-handler/index.mjs'
    ), 'is registered more than once')
  })

  it('applies the host event policy before invoking extension handlers', async () => {
    const handlers: ActionHandler[] = []
    const chat = {
      onAction(_actionId: string | string[], handler: ActionHandler) {
        handlers.push(handler)
      }
    } as unknown as Chat
    let handled = 0

    await registerSlackbotExtension(
      { id: 'guarded', actionIds: ['guarded.resolve'] },
      ({ chat: scopedChat }) => {
        scopedChat.onAction('guarded.resolve', async () => {
          handled += 1
        })
      },
      { chat, logger: logger([]) } as SlackbotExtensionContext,
      createSlackbotExtensionClaims(RESERVED_ACTION_PREFIX),
      '/overlay/guarded/index.mjs',
      raw => raw === 'allowed'
    )

    expect(handlers).toHaveLength(1)
    await handlers[0]!(actionEvent('denied'))
    await handlers[0]!(actionEvent('allowed'))
    expect(handled).toBe(1)
  })
})

function actionEvent(raw: unknown): ActionEvent {
  return { actionId: 'guarded.resolve', raw } as ActionEvent
}

async function expectRegistrationError(promise: Promise<void>, message: string): Promise<void> {
  try {
    await promise
    throw new Error('expected Slackbot extension registration to fail')
  } catch (error) {
    expect(error).toBeInstanceOf(Error)
    const cause = (error as Error & { cause?: unknown }).cause
    expect(cause).toBeInstanceOf(Error)
    expect((cause as Error).message).toContain(message)
  }
}

function logger(logs: string[]): Logger {
  const instance: Logger = {
    debug: message => logs.push(message),
    info: message => logs.push(message),
    warn: message => logs.push(message),
    error: message => logs.push(message),
    child: () => instance
  }
  return instance
}

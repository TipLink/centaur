import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import type {
  ActionHandler,
  Adapter,
  Chat,
  Logger,
  ModalSubmitHandler,
  OptionsLoadHandler,
  SlashCommandHandler
} from 'chat'
import type { SlackbotV2ThreadState } from './types'

export type SlackbotExtensionManifest = {
  actionIdPrefixes?: readonly string[]
  actionIds?: readonly string[]
  id: string
  modalCallbackIds?: readonly string[]
  optionsLoadIds?: readonly string[]
  slashCommands?: readonly string[]
}

export type SlackbotExtensionModuleConfig = {
  modulePath: string
  repositoryPath: string
  revision: string
}

export type SlackbotExtensionContext = {
  chat: SlackbotExtensionChat
  logger: Logger
}

export type SlackbotExtensionChat = Pick<
  Chat<Record<string, Adapter>, SlackbotV2ThreadState>,
  'onAction' | 'onModalSubmit' | 'onOptionsLoad' | 'onSlashCommand'
>

export type SlackbotExtensionRegister = (
  context: SlackbotExtensionContext
) => void | Promise<void>

export type SlackbotExtensionEventGuard = (
  raw: unknown
) => boolean | Promise<boolean>

export type SlackbotExtensionErrorReporter = (error: unknown) => void

type SlackbotExtensionModule = {
  extension?: SlackbotExtensionManifest
  register?: SlackbotExtensionRegister
}

export type SlackbotExtensionClaims = {
  claim(manifest: SlackbotExtensionManifest, modulePath: string): void
  ownsAction(actionId: string): boolean
  ownsModalCallback(callbackId: string): boolean
  ownsOptionsLoad(actionId: string): boolean
  ownsSlashCommand(command: string): boolean
}

export function createSlackbotExtensionClaims(
  reservedActionPrefix: string
): SlackbotExtensionClaims {
  const extensionIds = new Map<string, string>()
  const actionIds = new Map<string, string>()
  const actionPrefixes = new Map<string, string>()
  const modalCallbackIds = new Map<string, string>()
  const optionsLoadIds = new Map<string, string>()
  const slashCommands = new Map<string, string>()

  return {
    claim(manifest, modulePath) {
      const id = requiredIdentifier(manifest.id, 'extension id', modulePath)
      claimUnique(extensionIds, id, modulePath, 'extension id')

      const declaredActionIds = identifiers(manifest.actionIds, 'action ID', modulePath)
      const declaredOptionsLoadIds = identifiers(
        manifest.optionsLoadIds,
        'options-load ID',
        modulePath
      )
      const declaredPrefixes = identifiers(
        manifest.actionIdPrefixes,
        'action ID prefix',
        modulePath
      )
      for (const actionId of [...declaredActionIds, ...declaredOptionsLoadIds]) {
        assertActionClaimAvailable(
          actionId,
          id,
          modulePath,
          reservedActionPrefix,
          actionIds,
          actionPrefixes
        )
        actionIds.set(actionId, id)
      }
      for (const actionId of declaredOptionsLoadIds) optionsLoadIds.set(actionId, id)
      for (const prefix of declaredPrefixes) {
        assertActionPrefixAvailable(
          prefix,
          modulePath,
          reservedActionPrefix,
          actionIds,
          actionPrefixes
        )
        actionPrefixes.set(prefix, id)
      }
      for (const callbackId of identifiers(
        manifest.modalCallbackIds,
        'modal callback ID',
        modulePath
      )) {
        claimUnique(modalCallbackIds, callbackId, id, 'modal callback ID')
      }
      for (const command of identifiers(manifest.slashCommands, 'slash command', modulePath)) {
        if (!command.startsWith('/')) {
          throw new Error(
            `Slackbot extension slash command ${JSON.stringify(command)} in ${modulePath} must start with /`
          )
        }
        claimUnique(slashCommands, command, id, 'slash command')
      }
    },
    ownsAction(actionId) {
      return actionIds.has(actionId)
        || Array.from(actionPrefixes.keys()).some(prefix => actionId.startsWith(prefix))
    },
    ownsModalCallback(callbackId) {
      return modalCallbackIds.has(callbackId)
    },
    ownsOptionsLoad(actionId) {
      return optionsLoadIds.has(actionId)
    },
    ownsSlashCommand(command) {
      return slashCommands.has(command)
    }
  }
}

export function parseSlackbotExtensionModules(
  value: string | undefined
): SlackbotExtensionModuleConfig[] {
  if (!value?.trim()) return []

  let parsed: unknown
  try {
    parsed = JSON.parse(value)
  } catch (error) {
    throw new Error('SLACKBOTV2_EXTENSION_MODULES must be a JSON array of module descriptors', {
      cause: error
    })
  }
  if (!Array.isArray(parsed)) {
    throw new Error('SLACKBOTV2_EXTENSION_MODULES must be a JSON array of module descriptors')
  }

  const modules = parsed.map((entry, index) => extensionModuleConfig(entry, index))
  const paths = modules.map(module => module.modulePath)
  if (new Set(paths).size !== modules.length) {
    throw new Error('SLACKBOTV2_EXTENSION_MODULES must not contain duplicate module paths')
  }
  return modules
}

export async function loadSlackbotExtensions(
  modules: readonly SlackbotExtensionModuleConfig[],
  context: SlackbotExtensionContext,
  claims: SlackbotExtensionClaims,
  allowEvent: SlackbotExtensionEventGuard = () => true,
  reportError: SlackbotExtensionErrorReporter = () => undefined
): Promise<void> {
  for (const module of modules) {
    await verifyExtensionRevision(module)
    const resolvedPath = module.modulePath
    let loaded: SlackbotExtensionModule
    try {
      loaded = await import(pathToFileURL(resolvedPath).href) as SlackbotExtensionModule
    } catch (error) {
      throw new Error(`failed to import Slackbot extension module ${resolvedPath}`, { cause: error })
    }

    if (!isManifest(loaded.extension)) {
      throw new Error(`Slackbot extension module ${resolvedPath} must export an extension manifest`)
    }
    if (typeof loaded.register !== 'function') {
      throw new Error(`Slackbot extension module ${resolvedPath} must export register(context)`)
    }

    await registerSlackbotExtension(
      loaded.extension,
      loaded.register,
      context,
      claims,
      resolvedPath,
      allowEvent,
      reportError
    )
    context.logger.info('slackbotv2_extension_loaded', {
      extension_id: loaded.extension.id,
      module_path: resolvedPath
    })
  }
}

export async function verifyExtensionRevision(
  module: SlackbotExtensionModuleConfig
): Promise<void> {
  let head: string
  try {
    head = (await readFile(resolve(module.repositoryPath, '.git/HEAD'), 'utf8')).trim().toLowerCase()
  } catch (error) {
    throw new Error(
      `failed to read repository revision for Slackbot extension ${module.modulePath}`,
      { cause: error }
    )
  }
  if (head !== module.revision) {
    throw new Error(
      `Slackbot extension ${module.modulePath} requires revision ${module.revision}, found ${head}`
    )
  }
}

export async function registerSlackbotExtension(
  manifest: SlackbotExtensionManifest,
  register: SlackbotExtensionRegister,
  context: SlackbotExtensionContext,
  claims: SlackbotExtensionClaims,
  source: string,
  allowEvent: SlackbotExtensionEventGuard = () => true,
  reportError: SlackbotExtensionErrorReporter = () => undefined
): Promise<void> {
  try {
    claims.claim(manifest, source)
    const scoped = scopedExtensionContext(context, manifest, source, allowEvent, reportError)
    await register(scoped.context)
    scoped.assertComplete()
  } catch (error) {
    throw new Error(`failed to register Slackbot extension module ${source}`, { cause: error })
  }
}

function scopedExtensionContext(
  context: SlackbotExtensionContext,
  manifest: SlackbotExtensionManifest,
  source: string,
  allowEvent: SlackbotExtensionEventGuard,
  reportError: SlackbotExtensionErrorReporter
): { assertComplete(): void; context: SlackbotExtensionContext } {
  const declared = {
    actionIds: new Set(manifest.actionIds ?? []),
    actionIdPrefixes: new Set(manifest.actionIdPrefixes ?? []),
    modalCallbackIds: new Set(manifest.modalCallbackIds ?? []),
    optionsLoadIds: new Set(manifest.optionsLoadIds ?? []),
    slashCommands: new Set(manifest.slashCommands ?? [])
  }
  const registered = {
    actionIds: new Set<string>(),
    actionIdPrefixes: new Set<string>(),
    modalCallbackIds: new Set<string>(),
    optionsLoadIds: new Set<string>(),
    slashCommands: new Set<string>()
  }
  const base = context.chat
  const chat = new Proxy(base, {
    get(target, property) {
      if (property === 'onAction') {
        return (actionIdsOrHandler: string | string[] | ActionHandler, handler?: ActionHandler) => {
          if (typeof actionIdsOrHandler === 'function') {
            if (declared.actionIdPrefixes.size === 0) {
              throw undeclaredRegistration('action catch-all', source)
            }
            for (const prefix of declared.actionIdPrefixes) {
              if (registered.actionIdPrefixes.has(prefix)) {
                throw duplicateRegistration('action ID prefix', prefix, source)
              }
              registered.actionIdPrefixes.add(prefix)
            }
            target.onAction(async event => {
              if (Array.from(declared.actionIdPrefixes).some(prefix =>
                event.actionId.startsWith(prefix)
              ) && await allowEvent(event.raw)) {
                try {
                  await actionIdsOrHandler(event)
                } catch (error) {
                  reportError(error)
                  throw error
                }
              }
            })
            return
          }
          const actionIds = checkedRegistration(
            actionIdsOrHandler,
            declared.actionIds,
            registered.actionIds,
            'action ID',
            source
          )
          if (!handler) throw new Error(`Slackbot extension action handler is required in ${source}`)
          target.onAction(actionIds, async event => {
            if (!(await allowEvent(event.raw))) return
            try {
              await handler(event)
            } catch (error) {
              reportError(error)
              throw error
            }
          })
        }
      }
      if (property === 'onModalSubmit') {
        return (callbackIds: string | string[], handler: ModalSubmitHandler) => {
          const checked = checkedRegistration(
            callbackIds,
            declared.modalCallbackIds,
            registered.modalCallbackIds,
            'modal callback ID',
            source
          )
          target.onModalSubmit(checked, async event => {
            if (!(await allowEvent(event.raw))) return
            try {
              return await handler(event)
            } catch (error) {
              reportError(error)
              throw error
            }
          })
        }
      }
      if (property === 'onOptionsLoad') {
        return (actionIds: string | string[], handler: OptionsLoadHandler) => {
          const checked = checkedRegistration(
            actionIds,
            declared.optionsLoadIds,
            registered.optionsLoadIds,
            'options-load ID',
            source
          )
          target.onOptionsLoad(checked, async event => {
            if (!(await allowEvent(event.raw))) return
            try {
              return await handler(event)
            } catch (error) {
              reportError(error)
              throw error
            }
          })
        }
      }
      if (property === 'onSlashCommand') {
        return (
          commands: string | string[],
          handler: SlashCommandHandler<SlackbotV2ThreadState>
        ) => {
          const checked = checkedRegistration(
            commands,
            declared.slashCommands,
            registered.slashCommands,
            'slash command',
            source
          )
          target.onSlashCommand(checked, async event => {
            if (!(await allowEvent(event.raw))) return
            try {
              await handler(event)
            } catch (error) {
              reportError(error)
              throw error
            }
          })
        }
      }
      if (typeof property === 'string' && property.startsWith('on')) {
        throw new Error(
          `Slackbot extension registration method ${property} in ${source} is not supported`
        )
      }
      const value: unknown = Reflect.get(target, property, target)
      return typeof value === 'function' ? value.bind(target) : value
    }
  })

  return {
    context: { chat: chat as unknown as SlackbotExtensionChat, logger: context.logger },
    assertComplete() {
      assertRegistrationsComplete(declared.actionIds, registered.actionIds, 'action ID', source)
      assertRegistrationsComplete(
        declared.actionIdPrefixes,
        registered.actionIdPrefixes,
        'action ID prefix',
        source
      )
      assertRegistrationsComplete(
        declared.modalCallbackIds,
        registered.modalCallbackIds,
        'modal callback ID',
        source
      )
      assertRegistrationsComplete(
        declared.optionsLoadIds,
        registered.optionsLoadIds,
        'options-load ID',
        source
      )
      assertRegistrationsComplete(
        declared.slashCommands,
        registered.slashCommands,
        'slash command',
        source
      )
    }
  }
}

function checkedRegistration(
  input: string | string[],
  declared: Set<string>,
  registered: Set<string>,
  kind: string,
  source: string
): string[] {
  const identifiers = Array.isArray(input) ? input : [input]
  for (const identifier of identifiers) {
    if (!declared.has(identifier)) {
      throw undeclaredRegistration(`${kind} ${JSON.stringify(identifier)}`, source)
    }
    if (registered.has(identifier)) {
      throw duplicateRegistration(kind, identifier, source)
    }
    registered.add(identifier)
  }
  return identifiers
}

function duplicateRegistration(kind: string, identifier: string, source: string): Error {
  return new Error(
    `Slackbot extension ${kind} ${JSON.stringify(identifier)} in ${source} is registered more than once`
  )
}

function undeclaredRegistration(kind: string, source: string): Error {
  return new Error(`Slackbot extension ${kind} in ${source} is not declared in its manifest`)
}

function assertRegistrationsComplete(
  declared: Set<string>,
  registered: Set<string>,
  kind: string,
  source: string
): void {
  const missing = Array.from(declared).filter(identifier => !registered.has(identifier))
  if (missing.length > 0) {
    throw new Error(
      `Slackbot extension ${kind}s in ${source} were declared but not registered: ${missing.join(', ')}`
    )
  }
}

function isManifest(value: unknown): value is SlackbotExtensionManifest {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function extensionModuleConfig(value: unknown, index: number): SlackbotExtensionModuleConfig {
  if (!isRecord(value)) {
    throw new Error(`SLACKBOTV2_EXTENSION_MODULES[${index}] must be an object`)
  }
  const modulePath = requiredConfigString(value.modulePath, index, 'modulePath')
  const repositoryPath = requiredConfigString(value.repositoryPath, index, 'repositoryPath')
  const revision = requiredConfigString(value.revision, index, 'revision').toLowerCase()
  if (!/^[0-9a-f]{40}$/.test(revision)) {
    throw new Error(
      `SLACKBOTV2_EXTENSION_MODULES[${index}].revision must be a full 40-character commit SHA`
    )
  }
  const resolvedModulePath = resolve(modulePath)
  const resolvedRepositoryPath = resolve(repositoryPath)
  if (
    resolvedModulePath !== resolvedRepositoryPath
    && !resolvedModulePath.startsWith(`${resolvedRepositoryPath}/`)
  ) {
    throw new Error(
      `SLACKBOTV2_EXTENSION_MODULES[${index}].modulePath must be inside repositoryPath`
    )
  }
  return {
    modulePath: resolvedModulePath,
    repositoryPath: resolvedRepositoryPath,
    revision
  }
}

function requiredConfigString(
  value: unknown,
  index: number,
  field: keyof SlackbotExtensionModuleConfig
): string {
  if (typeof value !== 'string' || !value.trim()) {
    throw new Error(`SLACKBOTV2_EXTENSION_MODULES[${index}].${field} must be a non-empty string`)
  }
  return value.trim()
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function identifiers(
  value: readonly string[] | undefined,
  kind: string,
  modulePath: string
): string[] {
  if (value === undefined) return []
  if (!Array.isArray(value)) {
    throw new Error(`Slackbot extension ${kind}s in ${modulePath} must be an array`)
  }
  const result = value.map(identifier => requiredIdentifier(identifier, kind, modulePath))
  if (new Set(result).size !== result.length) {
    throw new Error(`Slackbot extension ${kind}s in ${modulePath} must not contain duplicates`)
  }
  return result
}

function requiredIdentifier(value: unknown, kind: string, modulePath: string): string {
  if (typeof value !== 'string' || !value.trim()) {
    throw new Error(`Slackbot extension ${kind} in ${modulePath} must be a non-empty string`)
  }
  if (value !== value.trim()) {
    throw new Error(
      `Slackbot extension ${kind} ${JSON.stringify(value)} in ${modulePath} must not contain surrounding whitespace`
    )
  }
  return value
}

function claimUnique(
  claims: Map<string, string>,
  identifier: string,
  owner: string,
  kind: string
): void {
  const existing = claims.get(identifier)
  if (existing && existing !== owner) {
    throw new Error(
      `Slackbot extension ${kind} ${JSON.stringify(identifier)} is already claimed by ${existing}`
    )
  }
  claims.set(identifier, owner)
}

function assertActionClaimAvailable(
  actionId: string,
  owner: string,
  modulePath: string,
  reservedPrefix: string,
  actionIds: Map<string, string>,
  actionPrefixes: Map<string, string>
): void {
  if (actionId.startsWith(reservedPrefix)) {
    throw new Error(
      `Slackbot extension action ID ${JSON.stringify(actionId)} in ${modulePath} is reserved by Centaur`
    )
  }
  const exactOwner = actionIds.get(actionId)
  if (exactOwner && exactOwner !== owner) {
    throw new Error(
      `Slackbot extension action ID ${JSON.stringify(actionId)} is already claimed by ${exactOwner}`
    )
  }
  const prefixOwner = Array.from(actionPrefixes.entries()).find(([prefix]) =>
    actionId.startsWith(prefix)
  )
  if (prefixOwner) {
    throw new Error(
      `Slackbot extension action ID ${JSON.stringify(actionId)} overlaps prefix claimed by ${prefixOwner[1]}`
    )
  }
}

function assertActionPrefixAvailable(
  prefix: string,
  modulePath: string,
  reservedPrefix: string,
  actionIds: Map<string, string>,
  actionPrefixes: Map<string, string>
): void {
  if (prefix.startsWith(reservedPrefix) || reservedPrefix.startsWith(prefix)) {
    throw new Error(
      `Slackbot extension action ID prefix ${JSON.stringify(prefix)} in ${modulePath} overlaps Centaur's reserved prefix`
    )
  }
  const exactOwner = Array.from(actionIds.entries()).find(([actionId]) => actionId.startsWith(prefix))
  if (exactOwner) {
    throw new Error(
      `Slackbot extension action ID prefix ${JSON.stringify(prefix)} overlaps an ID claimed by ${exactOwner[1]}`
    )
  }
  const prefixOwner = Array.from(actionPrefixes.entries()).find(([claimedPrefix]) =>
    prefix.startsWith(claimedPrefix) || claimedPrefix.startsWith(prefix)
  )
  if (prefixOwner) {
    throw new Error(
      `Slackbot extension action ID prefix ${JSON.stringify(prefix)} overlaps a prefix claimed by ${prefixOwner[1]}`
    )
  }
}

import { pathToFileURL } from 'node:url'
import type { Hono } from 'hono'
import type { StateAdapter } from 'chat'
import {
  CommandFieldError,
  type SlackCommandDefinition
} from './slack-command-registry'
import type { SlackbotV2Options } from './types'
import { verifySlackSignature } from './slack-commands'

export type SlackCommandExtensionContext = {
  app: Hono
  options: SlackbotV2Options
  state: StateAdapter
  verifySlackSignature: typeof verifySlackSignature
}

export type SlackCommandExtension = {
  commands: readonly SlackCommandDefinition[]
  mount?: (context: SlackCommandExtensionContext) => void
}

export async function loadSlackCommandExtensions(
  paths: readonly string[]
): Promise<SlackCommandExtension[]> {
  const extensions: SlackCommandExtension[] = []
  for (const path of paths) {
    const module = await import(pathToFileURL(path).href)
    const factory = module.default ?? module.createSlackCommandExtension
    if (typeof factory !== 'function')
      throw new Error(
        `Slack command extension ${path} must export a factory function`
      )
    const extension = await factory({ CommandFieldError })
    if (!extension || !Array.isArray(extension.commands))
      throw new Error(
        `Slack command extension ${path} must export commands as an array`
      )
    if (extension.mount !== undefined && typeof extension.mount !== 'function')
      throw new Error(
        `Slack command extension ${path} mount must be a function when provided`
      )
    extensions.push(extension as SlackCommandExtension)
  }
  return extensions
}

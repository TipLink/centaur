export const extension = {
  id: 'fixture',
  slashCommands: ['/fixture']
}

export function register({ chat, logger }) {
  chat.onSlashCommand('/fixture', async () => {})
  logger.info('fixture_extension_registered')
}

// Bot detail URL: `/bots/:name?tab=…`. Shared so the header switcher and the
// page itself keep the same tab when jumping between bots.

export const BOT_TABS = ['settings', 'dashboard', 'config', 'logs', 'tools'] as const
export type BotTab = (typeof BOT_TABS)[number]

export const TAB_LABEL: Record<BotTab, string> = {
  settings: 'Settings',
  dashboard: 'Dashboard',
  config: 'Raw config',
  logs: 'Logs',
  tools: 'Tools',
}

export function isBotTab(value: string | null | undefined): value is BotTab {
  return !!value && (BOT_TABS as readonly string[]).includes(value)
}

export function parseBotTab(
  value: string | null | undefined,
  fallback: BotTab = 'settings',
): BotTab {
  return isBotTab(value) ? value : fallback
}

export function botPath(name: string, tab: BotTab = 'settings'): string {
  return `/bots/${encodeURIComponent(name)}?tab=${tab}`
}

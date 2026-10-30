import { freshReading } from './curve'
import { freshUnwindReading } from './unwind'
import {
  moduleNames,
  modulesFor,
  runningConfigDiffers,
  liquidityState,
} from './modulePresentation'
import { moduleKeys, moduleMode, type ModuleKey } from './moduleModes'
import {
  moduleStatusFresh,
  type Mode,
  type ModulesConfig,
  type ModulesView,
} from './modules'
import type { PerpsView } from './api'
import type { BotTab } from './botRoutes'
import { hedgeMode, showHedgeModule } from './perps'

// Presentation only. Each module owns its configuration, mode and permissions.
// In particular, modulesEnabled gates quote policy, not FX hedging.
export const MODULE_SURFACES = {
  hedge: ['funds', 'dashboard', 'modules'],
  quotes: ['modules'],
} as const satisfies Record<string, readonly BotTab[]>
export type BotModuleId = keyof typeof MODULE_SURFACES

export function visibleBotModules(
  {
    quoteModulesEnabled,
    hedge,
    hedgeUnavailable = false,
    hedgeSetup = false,
  }: {
    quoteModulesEnabled: boolean
    hedge: PerpsView | null
    hedgeUnavailable?: boolean
    hedgeSetup?: boolean
  },
  now: number
): BotModuleId[] {
  const visible: Record<BotModuleId, boolean> = {
    hedge:
      hedgeSetup ||
      hedgeUnavailable ||
      Boolean(hedge?.assignmentId) ||
      showHedgeModule(hedge, now),
    quotes: quoteModulesEnabled,
  }
  return (Object.keys(MODULE_SURFACES) as BotModuleId[]).filter(
    (id) => visible[id]
  )
}

export function modulesOnTab(ids: readonly BotModuleId[], tab: BotTab) {
  return ids.filter((id) =>
    (MODULE_SURFACES[id] as readonly BotTab[]).includes(tab)
  )
}

export type ModuleSection = 'overview' | 'settings' | 'activity'
export type WorkspaceModuleId = 'hedge' | ModuleKey
export const MODULE_CATEGORIES = ['Capital', 'Pricing', 'Liquidity'] as const
export const WORKSPACE_MODULES: Record<
  WorkspaceModuleId,
  {
    name: string
    category: (typeof MODULE_CATEGORIES)[number]
    description: string
  }
> = {
  hedge: {
    name: 'FX hedge',
    category: 'Capital',
    description: 'Manage currency exposure and collateral-backed liquidity.',
  },
  curve: {
    name: 'Inventory curve',
    category: 'Pricing',
    description: 'Adjust quotes to the liquid inventory available to this bot.',
  },
  profit_unwind: {
    name: moduleNames.profit_unwind,
    category: 'Pricing',
    description:
      'Give part of a holding gain back as a better price, so excess inventory clears faster. Works on flat and curve pricing.',
  },
  inventory: {
    name: moduleNames.inventory,
    category: 'Pricing',
    description: 'Steer inventory toward a target by shifting quotes.',
  },
  spreads: {
    name: moduleNames.spreads,
    category: 'Pricing',
    description: 'Adjust quote margins as market prices move.',
  },
  rebalance: {
    name: moduleNames.rebalance,
    category: 'Liquidity',
    description: 'Sell excess inventory through the existing spot-sale flow.',
  },
  liquidity: {
    name: moduleNames.liquidity,
    category: 'Liquidity',
    description: 'Keep liquid balances aligned with market hours.',
  },
}

/** Eligibility comes from the server. Unknown quote funding never grants a module. */
export function workspaceModules(
  ids: readonly BotModuleId[],
  view: ModulesView | null
): WorkspaceModuleId[] {
  return [
    ...(ids.includes('hedge') ? ['hedge' as const] : []),
    ...(ids.includes('quotes') && view ? modulesFor(view.vault ?? true) : []),
  ]
}

/** Prefer the requested module, then a configured module. Navigation never writes. */
export function selectedWorkspaceModule(
  available: WorkspaceModuleId[],
  requested: string | null,
  view: ModulesView | null
): WorkspaceModuleId | null {
  return (
    available.find((id) => id === requested) ??
    available.find(
      (id) =>
        id === 'hedge' ||
        view?.config[id].enabled ||
        view?.status?.config[id]?.enabled
    ) ??
    available[0] ??
    null
  )
}

/** Each module saves its own table, its run mode included. */
export type QuoteConfigScope = ModuleKey

const configKeys = (scope: QuoteConfigScope): (keyof ModulesConfig)[] => [scope]

/** Construct the full API payload from one revision's saved config and one editor. */
export function quoteConfigForScope(
  saved: ModulesConfig,
  draft: ModulesConfig,
  scope: QuoteConfigScope
): ModulesConfig {
  return {
    ...saved,
    ...Object.fromEntries(configKeys(scope).map((key) => [key, draft[key]])),
  }
}

/** Carry unsaved sibling drafts forward after a successful scoped save. */
export function remainingQuoteDraft(
  saved: ModulesConfig,
  draft: ModulesConfig,
  next: ModulesConfig,
  scope: QuoteConfigScope
): ModulesConfig {
  const owned = configKeys(scope)
  const keys: readonly (keyof ModulesConfig)[] = moduleKeys
  return {
    ...next,
    ...Object.fromEntries(
      keys
        .filter(
          (key) =>
            !owned.includes(key) &&
            JSON.stringify(saved[key]) !== JSON.stringify(draft[key])
        )
        .map((key) => [key, draft[key]])
    ),
  }
}

export function quoteModuleUnsaved(
  view: ModulesView,
  draft: ModulesConfig,
  id: ModuleKey
): boolean {
  return configKeys(id).some(
    (key) => JSON.stringify(view.config[key]) !== JSON.stringify(draft[key])
  )
}

export function quoteModuleStatus(
  view: ModulesView,
  key: ModuleKey,
  now: number
): string {
  if (!view.running) return 'Bot stopped'
  if (runningConfigDiffers(view)) return 'Update pending'
  const mode = moduleMode(view.config, key)
  if (mode === 'off') return 'Off'
  if (key === 'curve') {
    if (!freshReading(view.status?.curve, now)) return 'Waiting'
  } else if (key === 'profit_unwind') {
    if (!freshUnwindReading(view.status?.profit_unwind, now)) return 'Waiting'
  } else if (key === 'liquidity') {
    if (!liquidityState(view, now).current) return 'Waiting'
  } else if (!moduleStatusFresh(view.status, view.running, now)) return 'Stale'
  if (
    key !== 'curve' &&
    key !== 'profit_unwind' &&
    key !== 'liquidity' &&
    view.status?.decisions.at(-1)?.decision.blocked
  )
    return 'Blocked'
  return mode === 'shadow' ? 'Preview' : 'Live'
}

/** A sidebar badge: a module's saved mode, or `on` for a hedge that is
 * switched on while its remote mode can't be read. */
export type BadgeMode = Mode | 'on'
export function hedgeBadge(view: PerpsView | null): BadgeMode | null {
  if (!view) return null
  const mode = hedgeMode(view)
  return mode === 'preview' ? 'shadow' : mode === 'unknown' ? 'on' : mode
}

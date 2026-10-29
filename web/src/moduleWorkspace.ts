import { freshReading } from './curve'
import {
  moduleNames,
  modulesFor,
  runningConfigDiffers,
  liquidityState,
  type ModuleKey,
} from './modulePresentation'
import {
  moduleStatusFresh,
  type ModulesConfig,
  type ModulesView,
} from './modules'
import type { PerpsView } from './api'
import type { BotTab } from './botRoutes'
import { showHedgeModule } from './perps'

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
    hedge: hedgeSetup || hedgeUnavailable || showHedgeModule(hedge, now),
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

/** A single configured module opens directly, even when other modules are eligible. */
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

/** Keep configured, running and drafted modules in view. Others live in Manage modules. */
export function listedModules(
  available: WorkspaceModuleId[],
  selected: WorkspaceModuleId | null,
  view: ModulesView | null,
  draft: ModulesConfig | null
) {
  return available.filter(
    (id) =>
      id === 'hedge' ||
      id === selected ||
      view?.config[id].enabled ||
      view?.status?.config[id]?.enabled ||
      draft?.[id].enabled
  )
}

/** The config blocks a quote module owns. Profit unwind rides on the curve
 * and is edited in its section, so its block is the curve's too. */
const configKeys = (id: ModuleKey): (keyof ModulesConfig)[] =>
  id === 'curve' ? ['curve', 'profit_unwind'] : [id]

/** Whether the draft changes anything this module owns, or the shared mode. */
export function quoteModuleUnsaved(
  view: ModulesView,
  draft: ModulesConfig,
  id: ModuleKey
): boolean {
  return (
    view.config.mode !== draft.mode ||
    configKeys(id).some(
      (key) => JSON.stringify(view.config[key]) !== JSON.stringify(draft[key])
    )
  )
}

export function quoteModuleStatus(
  view: ModulesView,
  key: ModuleKey,
  now: number
): string {
  if (!view.running) return 'Bot stopped'
  if (runningConfigDiffers(view)) return 'Update pending'
  if (view.config.mode === 'off' || !view.config[key].enabled) return 'Off'
  if (key === 'curve') {
    if (!freshReading(view.status?.curve, now)) return 'Waiting'
  } else if (key === 'liquidity') {
    if (!liquidityState(view, now).current) return 'Waiting'
  } else if (!moduleStatusFresh(view.status, view.running, now)) return 'Stale'
  if (
    key !== 'curve' &&
    key !== 'liquidity' &&
    view.status?.decisions.at(-1)?.decision.blocked
  )
    return 'Blocked'
  return view.config.mode === 'shadow' ? 'Preview' : 'Live'
}

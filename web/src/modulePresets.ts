import type { ModulesConfig } from './modules'
import { percentage } from './modulePresentation'

export type PresetModule = 'inventory' | 'spreads'
type Preset = {
  id: string
  name: string
  inventory: Pick<
    ModulesConfig['inventory'],
    'target_bps' | 'max_bps' | 'max_skew_bps'
  >
  spreads: Required<Omit<ModulesConfig['spreads'], 'enabled'>>
  tradeoff: Record<PresetModule, string>
}

/** Explicit user-applied starting points, not runtime defaults or optimized
 * strategies. See docs/module-presets.md for evidence and parameter rationale.
 * Floors, switches, mode and dealer settings are deliberately not preset-owned. */
export const modulePresets: readonly Preset[] = [
  {
    id: 'competitive',
    name: 'Competitive',
    inventory: { target_bps: 5000, max_bps: 7500, max_skew_bps: 10 },
    spreads: {
      inventory_aware: true,
      window_secs: 300,
      warmup_secs: 120,
      multiplier: 0.5,
      max_extra_bps: 5,
    },
    tradeoff: {
      inventory: 'More room to trade, with more currency exposure.',
      spreads: 'Tighter quotes, with less buffer when prices move.',
    },
  },
  {
    id: 'balanced',
    name: 'Balanced',
    inventory: { target_bps: 3000, max_bps: 6000, max_skew_bps: 15 },
    spreads: {
      inventory_aware: true,
      window_secs: 900,
      warmup_secs: 120,
      multiplier: 1,
      max_extra_bps: 15,
    },
    tradeoff: {
      inventory: 'A middle ground between trading room and currency exposure.',
      spreads: 'A middle ground between tighter quotes and a larger buffer.',
    },
  },
  {
    id: 'defensive',
    name: 'Defensive',
    inventory: { target_bps: 2000, max_bps: 4000, max_skew_bps: 25 },
    spreads: {
      inventory_aware: true,
      window_secs: 1800,
      warmup_secs: 120,
      multiplier: 1.5,
      max_extra_bps: 25,
    },
    tradeoff: {
      inventory:
        'Less currency exposure as trades fill, but purchases pause sooner.',
      spreads: 'A larger, longer-lasting buffer, which may mean fewer trades.',
    },
  },
]

export function matchingPreset(config: ModulesConfig, module: PresetModule) {
  return modulePresets.find((preset) =>
    Object.entries(preset[module]).every(
      ([key, value]) => Reflect.get(config[module], key) === value
    )
  )
}

export function presetConflict(
  config: ModulesConfig,
  module: PresetModule,
  preset: Preset
): string | null {
  const { target_bps, max_bps } = preset.inventory
  const trigger = config.rebalance.trigger_bps
  return module === 'inventory' &&
    config.rebalance.enabled &&
    (!Number.isInteger(trigger) || trigger <= target_bps || trigger > max_bps)
    ? `Requires a spot sale threshold above ${percentage(target_bps)} and at or below ${percentage(max_bps)}. Adjust it in Spot rebalancing first.`
    : null
}

export function applyPreset(
  config: ModulesConfig,
  module: PresetModule,
  preset: Preset
): ModulesConfig {
  // Guard the action as well as the control. Never silently move a live dealer
  // trigger when choosing an inventory policy with a different target or limit.
  if (presetConflict(config, module, preset)) return config
  return {
    ...config,
    [module]: { ...config[module], ...preset[module] },
  }
}

export function presetSummary(preset: Preset, module: PresetModule) {
  if (module === 'inventory') {
    const i = preset.inventory
    return `Target ${percentage(i.target_bps)} · Pause buying at ${percentage(i.max_bps)} · Adjust margins up to ${percentage(i.max_skew_bps)}`
  }
  const s = preset.spreads
  return `${s.window_secs / 60} min history · ${s.multiplier}× movement · Up to ${percentage(s.max_extra_bps)} extra margin`
}

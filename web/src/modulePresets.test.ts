import { describe, expect, it } from 'vitest'
import {
  applyPreset,
  matchingPreset,
  modulePresets,
  presetConflict,
} from './modulePresets'
import {
  inventoryIllustration,
  moduleParameterErrors,
  volatilityIllustration,
} from './modulePresentation'
import type { ModulesConfig } from './modules'

const config = (): ModulesConfig => ({
  mode: 'live',
  inventory: {
    enabled: false,
    target_bps: 2500,
    max_bps: 6500,
    max_skew_bps: 30,
    spread_floor_bps: 8,
  },
  spreads: {
    enabled: false,
    multiplier: 0.8,
    max_extra_bps: 8,
    window_secs: 600,
    warmup_secs: 60,
  },
  rebalance: {
    enabled: false,
    trigger_bps: 5000,
    max_trade_bps: 200,
    max_slippage_bps: 50,
    cooldown_secs: 300,
    order_lifetime_secs: 60,
    dealer: {
      url: 'https://dealer.example',
      taker: '0x123',
      api_key_env: 'DEALER_KEY',
    },
  },
})
const defensive = modulePresets.find((p) => p.id === 'defensive')!
const balanced = modulePresets.find((p) => p.id === 'balanced')!

describe('module preset boundaries', () => {
  it('changes only selected settings, preserving floors, mode, switches and dealer configuration', () => {
    for (const mode of ['live', 'off', 'shadow'] as const) {
      for (const module of ['inventory', 'spreads'] as const) {
        const original = { ...config(), mode }
        const before = structuredClone(original)
        const updated = applyPreset(original, module, defensive)
        expect(original).toEqual(before)
        expect(updated.mode).toBe(mode)
        expect(updated.inventory.enabled).toBe(false)
        expect(updated.spreads.enabled).toBe(false)
        expect(updated.inventory.spread_floor_bps).toBe(8)
        expect(updated.rebalance).toEqual(before.rebalance)
        const other = module === 'inventory' ? 'spreads' : 'inventory'
        expect(updated[other]).toEqual(before[other])
      }
    }
  })

  it('keeps every combination of the three profiles numerically valid', () => {
    expect(modulePresets).toHaveLength(3)
    for (const inventory of modulePresets) {
      for (const spreads of modulePresets) {
        const next = applyPreset(
          applyPreset(config(), 'inventory', inventory),
          'spreads',
          spreads
        )
        expect(moduleParameterErrors(next)).toEqual([])
      }
    }
  })

  it.each([2000, 4001, NaN])(
    'refuses an inventory profile incompatible with an enabled spot threshold of %s',
    (trigger) => {
      const original = config()
      original.rebalance.enabled = true
      original.rebalance.trigger_bps = trigger
      expect(presetConflict(original, 'inventory', defensive)).toContain(
        'above 20% and at or below 40%'
      )
      expect(applyPreset(original, 'inventory', defensive)).toBe(original)
      // Spread settings don't touch the target or trigger, even with an unrelated error.
      expect(presetConflict(original, 'spreads', defensive)).toBeNull()
    }
  )

  it.each([2001, 4000])(
    'allows a compatible enabled threshold of %s without editing it',
    (trigger) => {
      const original = config()
      original.rebalance.enabled = true
      original.rebalance.trigger_bps = trigger
      const next = applyPreset(original, 'inventory', defensive)
      expect(moduleParameterErrors(next)).toEqual([])
      expect(next.rebalance).toEqual(original.rebalance)
      expect(matchingPreset(next, 'inventory')?.id).toBe('defensive')
    }
  )

  it('recognizes settings after reload and switches to Custom when preset-owned values change', () => {
    const next: ModulesConfig = JSON.parse(
      JSON.stringify(applyPreset(config(), 'spreads', balanced))
    )
    expect(matchingPreset(next, 'spreads')?.id).toBe('balanced')
    next.spreads.warmup_secs += 1
    expect(matchingPreset(next, 'spreads')).toBeUndefined()
    expect(matchingPreset(config(), 'spreads')).toBeUndefined()
    const i = applyPreset(config(), 'inventory', defensive)
    i.inventory.spread_floor_bps = 12
    i.inventory.enabled = true
    expect(matchingPreset(i, 'inventory')?.id).toBe('defensive')
    i.inventory.max_skew_bps += 1
    expect(matchingPreset(i, 'inventory')).toBeUndefined()
  })
})

describe('preset strategy behavior', () => {
  it('makes Defensive pause purchases sooner but still buy toward target when underweight', () => {
    const original = config()
    original.inventory.enabled = true
    const next = applyPreset(original, 'inventory', defensive)
    expect(inventoryIllustration(next, 4000).buy).toBeNull()
    expect(inventoryIllustration(next, 4000).sell).toBe(8)
    expect(inventoryIllustration(next, 0).buy).toBe(8)
    expect(inventoryIllustration(next, 0).sell).toBeGreaterThan(20)
  })

  it('caps buffers and favors inventory reduction without enabling inventory implicitly', () => {
    const original = config()
    original.spreads.enabled = true
    const next = applyPreset(
      applyPreset(original, 'inventory', defensive),
      'spreads',
      defensive
    )
    // Without inventory enabled, the runtime uses equal buffers.
    expect(volatilityIllustration(next, 3000, 1000)).toEqual({
      buy: 25,
      sell: 25,
    })
    next.inventory.enabled = true
    expect(volatilityIllustration(next, 3000, 1000)).toEqual({
      buy: 25,
      sell: 13,
    })
    expect(volatilityIllustration(next, 4000, 1000)).toEqual({
      buy: null,
      sell: 0,
    })
    expect(volatilityIllustration(next, 2000, 0)).toEqual({ buy: 0, sell: 0 })
  })
})

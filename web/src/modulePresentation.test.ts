import { describe, expect, it } from 'vitest'
import type { ModulesConfig, ModulesView } from './modules'
import {
  corridorSpreadBps,
  corridorSpreads,
  inventoryIllustration,
  moduleChanges,
  moduleKeyFigures,
  moduleOverviewState,
  moduleParameterErrors,
  quoteBreakdown,
  quoteIllustration,
  quoteModeNote,
  volatilityIllustration,
} from './modulePresentation'
import { applyPreset, modulePresets } from './modulePresets'

const config = (): ModulesConfig => ({
  mode: 'live',
  inventory: {
    enabled: true,
    target_bps: 4000,
    max_bps: 8000,
    max_skew_bps: 40,
    spread_floor_bps: 5,
  },
  spreads: {
    enabled: true,
    inventory_aware: true,
    multiplier: 1,
    max_extra_bps: 100,
    window_secs: 300,
    warmup_secs: 30,
  },
  rebalance: {
    enabled: false,
    trigger_bps: 5000,
    max_trade_bps: 200,
    max_slippage_bps: 50,
    cooldown_secs: 300,
    order_lifetime_secs: 60,
    dealer: null,
  },
})

describe('module parameter validation', () => {
  it('accepts exact bps values and fractional sensitivity without changing the config', () => {
    const c = config()
    c.inventory.target_bps = 2525
    c.spreads.multiplier = 0.008
    const before = structuredClone(c)
    expect(moduleParameterErrors(c)).toEqual([])
    expect(c).toEqual(before)
  })
  it('rejects empty, fractional-bps and non-finite inputs, including disabled modules', () => {
    for (const value of [NaN, Infinity, -1, 5000, 1.1]) {
      const c = config()
      c.spreads.enabled = false
      c.spreads.max_extra_bps = value
      expect(moduleParameterErrors(c)).toHaveLength(1)
    }
  })
  it('rejects inconsistent target, timing and enabled rebalance thresholds', () => {
    const c = config()
    c.inventory.target_bps = c.inventory.max_bps
    c.spreads.warmup_secs = c.spreads.window_secs
    c.rebalance.cooldown_secs = c.rebalance.order_lifetime_secs + 29
    expect(moduleParameterErrors(c)).toHaveLength(3)
    c.rebalance.enabled = true
    expect(moduleParameterErrors(c)).toHaveLength(4)
  })
})

describe('interactive illustrations match strategy rounding', () => {
  const base = { buy: 20, sell: 20 }
  // Matching the Rust evaluator's fixed-share regression cases, which start
  // from a 20 bps corridor spread on both sides.
  it.each([
    [0, 5, 70],
    [2000, 10, 50],
    [4000, 30, 30],
    [6000, 50, 10],
    [8000, null, 5],
    [9945, null, 5],
    [10000, null, 5],
  ])('prices the %i bps inventory example correctly', (share, buy, sell) => {
    expect(quoteIllustration(config(), base, share!, 10).quoted).toEqual({
      buy,
      sell,
    })
  })
  it('keeps purchase vetoes even with symmetric buffers, and applies caps before weighting', () => {
    const c = config()
    c.spreads.inventory_aware = false
    expect(volatilityIllustration(c, 9945, 20)).toEqual({ buy: null, sell: 20 })
    c.spreads.inventory_aware = true
    c.spreads.max_extra_bps = 6
    expect(volatilityIllustration(c, 6000, 20)).toEqual({ buy: 6, sell: 3 })
    expect(volatilityIllustration(c, 6000, 0.1)).toEqual({ buy: 1, sell: 1 })
    c.inventory.enabled = false
    expect(inventoryIllustration(c, 9945, base)).toEqual({ buy: 20, sell: 20 })
    expect(volatilityIllustration(c, 9945, 20)).toEqual({ buy: 6, sell: 6 })
  })
})

describe('the simulated spread starts from the corridor spread', () => {
  it('reads bps as set and converts an absolute offset at the price, rounding up', () => {
    expect(
      corridorSpreadBps({ kind: 'bps', value: 35 }, 'buy', undefined)
    ).toBe(35)
    expect(corridorSpreadBps(null, 'sell', 1)).toBeNull()
    expect(
      corridorSpreadBps({ kind: 'abs', value: 10 }, 'buy', undefined)
    ).toBeUndefined()
    // 1,000 corridor per settlement token, 10 more or fewer on each side.
    expect(corridorSpreadBps({ kind: 'abs', value: 10 }, 'buy', 0.001)).toBe(
      100
    )
    expect(corridorSpreadBps({ kind: 'abs', value: 10 }, 'sell', 0.001)).toBe(
      102
    )
    expect(
      corridorSpreads(
        { buy: { kind: 'bps', value: 5 }, sell: { kind: 'abs', value: 10 } },
        undefined
      )
    ).toBeNull()
    expect(
      corridorSpreads(
        { buy: null, sell: { kind: 'bps', value: 40 } },
        undefined
      )
    ).toEqual({ buy: null, sell: 40 })
  })
  it('stacks inventory balancing and dynamic spreads on an asymmetric corridor', () => {
    // 60% held, 20 bps range: inventory moves both sides 20 bps, dynamic
    // spreads add 20 to buys and 10 to sells after inventory weighting.
    const c = config()
    const base = { buy: 10, sell: 60 }
    expect(quoteIllustration(c, base, 6000, 20)).toEqual({
      base,
      inventory: { buy: 30, sell: 40 },
      quoted: { buy: 50, sell: 50 },
    })
    expect(quoteBreakdown(c, base, 6000, 20)).toEqual([
      {
        key: 'corridor',
        label: 'Corridor spread',
        on: true,
        buy: '0.1%',
        sell: '0.6%',
      },
      {
        key: 'inventory',
        label: 'Inventory balancing',
        on: true,
        buy: '+0.2%',
        sell: '−0.2%',
      },
      {
        key: 'spreads',
        label: 'Dynamic spreads',
        on: true,
        buy: '+0.2%',
        sell: '+0.1%',
      },
      {
        key: 'quoted',
        label: 'Quoted spread',
        on: true,
        buy: '0.5%',
        sell: '0.5%',
      },
    ])
  })
  it('applies the minimum margin to the corridor spread, not to a fixed example', () => {
    const c = config()
    c.spreads.enabled = false
    c.inventory.spread_floor_bps = 15
    expect(quoteIllustration(c, { buy: 50, sell: 12 }, 6000, 0).quoted).toEqual(
      {
        buy: 70,
        sell: 15,
      }
    )
  })
  it('keeps a side the corridor leaves off and pauses buys at the purchase limit', () => {
    const c = config()
    expect(
      quoteBreakdown(c, { buy: null, sell: 30 }, 4000, 0).map((r) => r.buy)
    ).toEqual(['Off', '—', '—', 'Off'])
    expect(
      quoteBreakdown(c, { buy: 30, sell: 30 }, 8000, 0).map((r) => r.buy)
    ).toEqual(['0.3%', 'Paused', '—', 'Paused'])
  })
  it('passes the corridor spread through modules that are off, and caps buys', () => {
    const c = config()
    c.inventory.enabled = false
    c.spreads.enabled = false
    c.mode = 'shadow'
    expect(quoteBreakdown(c, { buy: 25, sell: 30 }, 6000, 50)).toEqual([
      {
        key: 'corridor',
        label: 'Corridor spread',
        on: true,
        buy: '0.25%',
        sell: '0.3%',
      },
      {
        key: 'inventory',
        label: 'Inventory balancing',
        on: false,
        buy: 'Off',
        sell: 'Off',
      },
      {
        key: 'spreads',
        label: 'Dynamic spreads',
        on: false,
        buy: 'Off',
        sell: 'Off',
      },
      {
        key: 'quoted',
        label: 'Proposed spread',
        on: true,
        buy: '0.25%',
        sell: '0.3%',
      },
    ])
    c.spreads.enabled = true
    expect(
      quoteIllustration(c, { buy: 9990, sell: 9990 }, 6000, 50).quoted
    ).toEqual({
      buy: 9999,
      sell: 10040,
    })
  })
  it('says when the bot will not quote the simulated spread', () => {
    expect(quoteModeNote('live')).toBeNull()
    expect(quoteModeNote('shadow')).toMatch(
      /Live quotes stay at the corridor spread/
    )
    expect(quoteModeNote('off')).toMatch(
      /live quotes stay at the corridor spread/
    )
  })
})

const view = (): ModulesView => ({
  config: config(),
  revision: 'one',
  running: true,
  corridor_decimals: 18,
  settlement_decimals: 18,
  corridor_spreads: {
    buy: { kind: 'bps', value: 20 },
    sell: { kind: 'bps', value: 20 },
  },
  status: {
    version: 1,
    config: config(),
    at: 100,
    rebalance_status: 'Disabled',
    next_attempt_at: 0,
    decisions: [
      {
        price: 1,
        corridor: '1000',
        settlement: '1000',
        decision: {
          version: 1,
          at: 100,
          inventory_bps: 5000,
          buy_bps: 50,
          sell_bps: 5,
          buy_limit: '100',
          rebalance_sell: '0',
          volatility_bps: 10,
          reasons: [],
          blocked: false,
        },
      },
    ],
  },
})
describe('concise overview truthfulness', () => {
  it('withholds stale, future, disconnected and stopped readings', () => {
    for (const now of [99, 111])
      expect(moduleOverviewState(view(), now).current).toBe(false)
    const v = view()
    v.running = false
    expect(moduleOverviewState(v, 101).title).toBe('Ready when you are')
    v.running = true
    v.status!.quote_status = {
      at: 101,
      state: 'waiting_for_session',
      message: 'Disconnected',
    }
    expect(moduleOverviewState(v, 101).current).toBe(false)
  })
  it('never calls preview failures a pause in actual quotes', () => {
    const v = view()
    v.status!.config.mode = 'shadow'
    expect(moduleOverviewState(v, 101).title).toBe('Previewing your strategy')
    v.status!.decisions[0]!.decision.blocked = true
    expect(moduleOverviewState(v, 101).title).toBe('Preview is waiting')
    v.status!.config.mode = 'live'
    expect(moduleOverviewState(v, 101).title).toBe('Quotes are paused')
  })
  it('calls a live warm-up at the capped buffer getting ready, not paused', () => {
    const v = view()
    v.status!.config.mode = 'live'
    v.status!.decisions[0]!.decision.blocked = false
    v.status!.decisions[0]!.decision.reasons = [
      'Warming up dynamic spreads: holding the maximum buffer (buy +5 bps, sell +5 bps) until 120s of price history',
    ]
    expect(moduleOverviewState(v, 101).title).toBe('Getting ready')
    v.status!.config.mode = 'shadow'
    expect(moduleOverviewState(v, 101).title).toBe('Previewing your strategy')
  })
  it('distinguishes off, warmup and all modules disabled', () => {
    const v = view()
    v.status!.decisions[0]!.decision.blocked = true
    v.status!.decisions[0]!.decision.reasons = [
      'Collecting price history for dynamic spreads',
    ]
    expect(moduleOverviewState(v, 101).title).toBe('Getting ready')
    v.status!.config.mode = 'off'
    expect(moduleOverviewState(v, 101).current).toBe(false)
    expect(moduleOverviewState(v, 111).title).toBe('Waiting for an update')
    v.status!.config.mode = 'live'
    v.status!.decisions[0]!.decision.blocked = false
    for (const key of ['inventory', 'spreads', 'rebalance'] as const)
      v.status!.config[key].enabled = false
    expect(moduleOverviewState(v, 101).title).toBe('No modules enabled')
  })
})

describe('strategy rows and the save bar', () => {
  it('lists nothing when the draft matches the saved settings', () => {
    expect(moduleChanges(config(), config())).toEqual([])
  })
  it('surfaces preset changes to secondary settings, in form order', () => {
    const saved = config()
    const draft = applyPreset(saved, 'inventory', modulePresets[0]!)
    expect(moduleChanges(saved, draft)).toEqual([
      { label: 'Target holdings', from: '40%', to: '50%' },
      { label: 'Pause purchases at', from: '80%', to: '75%' },
      { label: 'Maximum price adjustment', from: '0.4%', to: '0.1%' },
    ])
    expect(saved).toEqual(config())
  })
  it('names modes and switches as the panel does', () => {
    const draft = config()
    draft.mode = 'shadow'
    draft.rebalance.enabled = true
    expect(moduleChanges(config(), draft)).toEqual([
      { label: 'Run mode', from: 'Live', to: 'Preview' },
      { label: 'Spot rebalancing', from: 'Off', to: 'On' },
    ])
  })
  it('treats a missing inventory weighting flag as off', () => {
    const saved = config()
    delete saved.spreads.inventory_aware
    const draft = config()
    draft.spreads.inventory_aware = false
    expect(moduleChanges(saved, draft)).toEqual([])
  })
  it('shows an emptied field as a dash, as one change', () => {
    const draft = config()
    draft.inventory.target_bps = NaN
    expect(moduleChanges(config(), draft)).toEqual([
      { label: 'Target holdings', from: '40%', to: '—' },
    ])
    expect(moduleKeyFigures(draft, 'inventory')[0]).toEqual({
      label: 'Target',
      value: '—',
    })
  })
  it('gives each module its two defining figures', () => {
    const c = config()
    expect(moduleKeyFigures(c, 'inventory')).toEqual([
      { label: 'Target', value: '40%' },
      { label: 'Pause at', value: '80%' },
    ])
    expect(moduleKeyFigures(c, 'spreads')).toEqual([
      { label: 'Max extra', value: '+1%' },
      { label: 'Sensitivity', value: '1×' },
    ])
    expect(moduleKeyFigures(c, 'rebalance')).toEqual([
      { label: 'Sell above', value: '50%' },
      { label: 'Per sale', value: '2%' },
    ])
  })
})

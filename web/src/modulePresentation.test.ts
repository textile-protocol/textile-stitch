import { describe, expect, it } from 'vitest'
import type { ModulesConfig, ModulesView } from './modules'
import {
  inventoryIllustration,
  moduleOverviewState,
  moduleParameterErrors,
  volatilityIllustration,
} from './modulePresentation'

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
  // Matching the Rust evaluator's fixed-share regression cases. The UI shows
  // one module at a time; adding their effects must reproduce its spreads.
  it.each([
    [0, 5, 70],
    [2000, 10, 50],
    [4000, 30, 30],
    [6000, 50, 10],
    [8000, null, 5],
    [9945, null, 5],
    [10000, null, 5],
  ])('prices the %i bps inventory example correctly', (share, buy, sell) => {
    const c = config()
    const inventory = inventoryIllustration(c, share!)
    const extra = volatilityIllustration(c, share!, 10)
    expect(
      inventory.buy === null || extra.buy === null
        ? null
        : inventory.buy + extra.buy
    ).toBe(buy)
    expect(inventory.sell + extra.sell).toBe(sell)
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
    expect(inventoryIllustration(c, 9945)).toEqual({ buy: 20, sell: 20 })
    expect(volatilityIllustration(c, 9945, 20)).toEqual({ buy: 6, sell: 6 })
  })
})

const view = (): ModulesView => ({
  config: config(),
  revision: 'one',
  running: true,
  dataset_template: {},
  settlement_decimals: 18,
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

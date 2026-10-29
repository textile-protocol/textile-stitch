import { describe, expect, it } from 'vitest'
import type { LiquidityStatus, ModulesConfig, ModulesView } from './modules'
import {
  corridorSpreadBps,
  corridorSpreads,
  defaultCurve,
  defaultLiquidity,
  inventoryIllustration,
  liquiditySchedule,
  liquiditySetupHint,
  liquidityState,
  liveReading,
  liveRow,
  marketMinutes,
  moduleChanges,
  moduleOverviewState,
  moduleParameterErrors,
  quoteBreakdown,
  quoteIllustration,
  quoteModeNote,
  runnableConfig,
  runningConfigDiffers,
  scheduleByDay,
  shareBps,
  shiftClock,
  volatilityIllustration,
  warmupNote,
  windowRangeBps,
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
  liquidity: defaultLiquidity(),
  curve: defaultCurve(),
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
  it('names the capped start dynamic spreads quotes before it has price history', () => {
    const c = config()
    const base = { buy: 20, sell: 20 }
    // The full 100 bps, weighted at 60% held: buys +100, sells +50.
    expect(quoteIllustration(c, base, 6000, null).quoted).toEqual({
      buy: 140,
      sell: 55,
    })
    expect(warmupNote(c, base, 6000, 20)).toBe(
      'Until the bot has 30 sec of price history, dynamic spreads add their full maximum: buys 1.4%, sells 0.55%. Expect that right after switching from Off, or after a stop longer than the 300 sec window.'
    )
    // At the purchase limit weighting drops the sell buffer too, so the
    // start matches the table; equal buffers keep the full cap on sells.
    expect(warmupNote(c, base, 8000, 20)).toBeNull()
    expect(
      warmupNote(
        { ...c, spreads: { ...c.spreads, inventory_aware: false } },
        base,
        8000,
        20
      )
    ).toMatch(/buys paused, sells 1.05%/)
    // Already at the cap, or nothing live to warm up.
    expect(warmupNote(c, base, 6000, 1000)).toBeNull()
    c.mode = 'shadow'
    expect(warmupNote(c, base, 6000, 20)).toBeNull()
    c.mode = 'live'
    c.spreads.enabled = false
    expect(warmupNote(c, base, 6000, 20)).toBeNull()
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
// The cNGN vault from the Oct 16 report: a 1/1 bps corridor, target 0.01%,
// purchases paused at 75%, 0.1% maximum adjustment, 0.01% floor, and dynamic
// spreads at 0.5× up to 0.05% with inventory weighting. Holding 0%, it quoted
// 0.01% / 0.02% while the Simulate default (45% held, 0.2% range) said
// 0.11% / 0.04%.
const cngnVault = (): ModulesView => {
  const c = config()
  c.inventory = {
    enabled: true,
    target_bps: 1,
    max_bps: 7500,
    max_skew_bps: 10,
    spread_floor_bps: 1,
  }
  c.spreads = {
    enabled: true,
    inventory_aware: true,
    multiplier: 0.5,
    max_extra_bps: 5,
    window_secs: 300,
    warmup_secs: 120,
  }
  return {
    ...view(),
    config: c,
    corridor_spreads: {
      buy: { kind: 'bps', value: 1 },
      sell: { kind: 'bps', value: 1 },
    },
    status: {
      version: 1,
      config: c,
      at: 100,
      rebalance_status: 'Disabled',
      next_attempt_at: 0,
      decisions: [
        {
          price: 0.00073,
          corridor: '0',
          settlement: '1000',
          decision: {
            version: 1,
            at: 100,
            inventory_bps: 0,
            buy_bps: 1,
            sell_bps: 2,
            buy_limit: '1',
            rebalance_sell: '0',
            volatility_bps: 1,
            reasons: [],
            blocked: false,
          },
          inputs: {
            price_at: 100,
            balances_at: 100,
            base_buy_bps: 1,
            base_sell_bps: 1,
            inventory_buy_bps: 1,
            inventory_sell_bps: 1,
            // A 1.81 bps range: × 0.5 rounds up to 1 bps.
            spread_window: {
              samples: 40,
              history_secs: 290,
              low: 0.0011209,
              high: 0.0011211025823673593,
              extra_bps: 1,
            },
          },
        },
      ],
    },
  }
}

describe('the Simulate table starts from what the bot sees now', () => {
  it('reads the range the way the bot measures it, null while warming up', () => {
    expect(windowRangeBps({ low: 1, high: 1.001953125, extra_bps: 20 })).toBe(
      19.53125
    )
    expect(windowRangeBps({ low: 1, high: 1.5, extra_bps: null })).toBeNull()
  })
  it('reproduces the quote the bot is making from its own reading', () => {
    const v = cngnVault()
    const now = liveReading(v, 101)!
    expect(now).toEqual({
      share: 0,
      rangeBps: (0.0011211025823673593 / 0.0011209 - 1) * 10000,
      quoted: { buy: 1, sell: 2 },
      base: { buy: 1, sell: 1 },
      mode: 'live',
    })
    const base = corridorSpreads(
      v.corridor_spreads,
      v.status!.decisions[0]!.price
    )!
    expect(
      quoteIllustration(v.config, base, now.share, now.rangeBps).quoted
    ).toEqual(now.quoted)
    expect(liveRow(now)).toEqual({
      label: 'Quoting now',
      buy: '0.01%',
      sell: '0.02%',
    })
    // The old fixed example inputs are what showed the higher spread.
    expect(quoteIllustration(v.config, base, 4500, 20).quoted).toEqual({
      buy: 11,
      sell: 4,
    })
    expect(quoteIllustration(v.config, base, 0, 20).quoted).toEqual({
      buy: 1,
      sell: 6,
    })
  })
  it('has no reading for a stale, blocked, off or input-less evaluation', () => {
    expect(liveReading(cngnVault(), 111)).toBeNull()
    const blocked = cngnVault()
    blocked.status!.decisions[0]!.decision.blocked = true
    expect(liveReading(blocked, 101)).toBeNull()
    const off = cngnVault()
    off.status!.config = { ...off.status!.config, mode: 'off' }
    expect(liveReading(off, 101)).toBeNull()
    expect(liveReading(view(), 101)).toBeNull()
  })
  it('names a paused or off side and a preview', () => {
    const v = cngnVault()
    v.status!.config = { ...v.status!.config, mode: 'shadow' }
    v.status!.decisions[0]!.decision.buy_bps = null
    v.status!.decisions[0]!.inputs!.base_sell_bps = null
    v.status!.decisions[0]!.decision.sell_bps = null
    expect(liveRow(liveReading(v, 101)!)).toEqual({
      label: 'Proposed now',
      buy: 'Paused',
      sell: 'Off',
    })
    v.status!.decisions[0]!.inputs!.spread_window.extra_bps = null
    expect(liveReading(v, 101)!.rangeBps).toBeNull()
  })
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
  })
  it('lists liquidity schedule changes with the form labels', () => {
    const draft = config()
    draft.liquidity.enabled = true
    draft.liquidity.days = ['mon', 'tue']
    expect(moduleChanges(config(), draft)).toEqual([
      { label: 'Liquidity schedule', from: 'Off', to: 'On' },
      {
        label: 'Market days (UTC)',
        from: 'Mon, Tue, Wed, Thu, Fri',
        to: 'Mon, Tue',
      },
    ])
  })
  it.each(['IDRX per USDT', 'cNGN per USDT', 'wBRL per USDC'])(
    'shows curve offset changes in %s without converting the saved value',
    (unit) => {
      const saved = config()
      const draft = { ...saved, curve: { ...saved.curve, start_below_mid: 2 } }
      expect(moduleChanges(saved, draft, unit)).toEqual([
        { label: 'Start below the mid', from: `0 ${unit}`, to: `2 ${unit}` },
      ])
    }
  )
})

const ROLES = '0x' + 'ab'.repeat(20)
const ROLE_KEY = '0x' + '01'.repeat(32)

describe('liquidity schedule validation', () => {
  it('accepts the server defaults in every mode while disabled', () => {
    for (const mode of ['off', 'shadow', 'live'] as const)
      expect(moduleParameterErrors({ ...config(), mode })).toEqual([])
  })
  it('checks clock format and rejects equal open and close times', () => {
    for (const [open, close] of [
      ['8:00', '17:00'],
      ['24:00', '17:00'],
      ['08:60', '17:00'],
      ['08:00', '08:00'],
      ['', '17:00'],
    ]) {
      const c = config()
      c.liquidity.open_utc = open!
      c.liquidity.close_utc = close!
      expect(moduleParameterErrors(c)).toEqual([
        'Liquidity schedule: enter opening and closing times as HH:MM in UTC, and make them different.',
      ])
    }
    const c = config()
    c.liquidity.open_utc = '23:59'
    c.liquidity.close_utc = '00:00'
    expect(moduleParameterErrors(c)).toEqual([])
  })
  it('needs at least one day and no duplicates', () => {
    const c = config()
    c.liquidity.days = []
    expect(moduleParameterErrors(c)).toHaveLength(1)
    c.liquidity.days = ['mon', 'mon']
    expect(moduleParameterErrors(c)).toHaveLength(1)
  })
  it('bounds floors to whole bps and keeps market hours at or above off-hours', () => {
    for (const value of [-1, 10001, 0.5, NaN]) {
      const c = config()
      c.liquidity.market_floor_bps = value
      c.liquidity.off_hours_floor_bps = 0
      expect(moduleParameterErrors(c)).toEqual([
        'Liquidity schedule: floors must be 0–100% of free settlement, in steps of 0.01%.',
      ])
    }
    const c = config()
    c.liquidity.market_floor_bps = 10000
    c.liquidity.off_hours_floor_bps = 10000
    expect(moduleParameterErrors(c)).toEqual([])
    c.liquidity.market_floor_bps = 9999
    expect(moduleParameterErrors(c)).toEqual([
      'Liquidity schedule: keep at least as much liquid during market hours as off-hours.',
    ])
  })
  it('keeps tolerance within 1–5000 bps', () => {
    for (const [value, errors] of [
      [0, 1],
      [1, 0],
      [5000, 0],
      [5001, 1],
      [2.5, 1],
    ] as const) {
      const c = config()
      c.liquidity.tolerance_bps = value
      expect(moduleParameterErrors(c)).toHaveLength(errors)
    }
  })
  it('checks authority format whenever one is entered, even while disabled', () => {
    const c = config()
    c.liquidity.authority = { roles_modifier: ROLES, role_key: ROLE_KEY }
    expect(moduleParameterErrors(c)).toEqual([])
    for (const roles_modifier of [
      '',
      ROLES.slice(0, -1),
      ROLES.slice(2),
      '0x' + '0'.repeat(40),
      '0x' + 'g'.repeat(40),
    ]) {
      c.liquidity.authority = { roles_modifier, role_key: ROLE_KEY }
      expect(moduleParameterErrors(c)).toHaveLength(1)
    }
    for (const role_key of ['', '0x' + '0'.repeat(64), ROLE_KEY + '0']) {
      c.liquidity.authority = { roles_modifier: ROLES, role_key }
      expect(moduleParameterErrors(c)).toHaveLength(1)
    }
  })
  it('requires an authority only when enabled in Live mode', () => {
    const c = config()
    c.liquidity.enabled = true
    expect(moduleParameterErrors(c)).toEqual([
      'Liquidity schedule: Live mode needs the Roles modifier and role key that let this bot change the floor.',
    ])
    expect(moduleParameterErrors({ ...c, mode: 'shadow' })).toEqual([])
    c.liquidity.authority = { roles_modifier: ROLES, role_key: ROLE_KEY }
    expect(moduleParameterErrors(c)).toEqual([])
  })
})

describe('liquidity schedule illustration', () => {
  it('covers the week with market hours on each listed day', () => {
    const segments = liquiditySchedule(defaultLiquidity())!
    expect(segments[0]).toEqual({
      start: 0,
      end: 480,
      market: false,
      floor_bps: 0,
    })
    expect(segments[1]).toEqual({
      start: 480,
      end: 1020,
      market: true,
      floor_bps: 5000,
    })
    expect(segments.at(-1)).toEqual({
      start: 4 * 1440 + 1020,
      end: 7 * 1440,
      market: false,
      floor_bps: 0,
    })
    expect(marketMinutes(segments)).toBe(5 * 9 * 60)
    segments.forEach((s, i) => expect(s.start).toBe(segments[i - 1]?.end ?? 0))
  })
  it('runs a window past midnight into the next day', () => {
    const l = {
      ...defaultLiquidity(),
      open_utc: '22:00',
      close_utc: '06:00',
      days: ['fri' as const],
    }
    expect(liquiditySchedule(l)!.filter((s) => s.market)).toEqual([
      {
        start: 4 * 1440 + 1320,
        end: 5 * 1440 + 360,
        market: true,
        floor_bps: 5000,
      },
    ])
    const days = scheduleByDay(liquiditySchedule(l)!)
    expect(days[4]!.segments.at(-1)).toMatchObject({
      start: 1320,
      end: 1440,
      market: true,
    })
    expect(days[5]!.segments[0]).toMatchObject({
      start: 0,
      end: 360,
      market: true,
    })
  })
  it('wraps a Sunday overnight window into Monday morning', () => {
    const l = {
      ...defaultLiquidity(),
      open_utc: '22:00',
      close_utc: '06:00',
      days: ['sun' as const],
    }
    const segments = liquiditySchedule(l)!
    expect(segments[0]).toMatchObject({ start: 0, end: 360, market: true })
    expect(segments.at(-1)).toMatchObject({
      start: 6 * 1440 + 1320,
      end: 7 * 1440,
      market: true,
    })
    expect(marketMinutes(segments)).toBe(480)
  })
  it('returns nothing for invalid times', () => {
    expect(
      liquiditySchedule({ ...defaultLiquidity(), close_utc: '08:00' })
    ).toBeNull()
  })
  it('converts UTC clock times to the operator offset with a day shift', () => {
    expect(shiftClock('08:00', 120)).toEqual({ time: '10:00', dayShift: 0 })
    expect(shiftClock('23:30', 60)).toEqual({ time: '00:30', dayShift: 1 })
    expect(shiftClock('02:00', -300)).toEqual({ time: '21:00', dayShift: -1 })
    expect(shiftClock('nope', 0)).toBeNull()
  })
  it('measures shares of free settlement without float rounding', () => {
    expect(
      shareBps('500000000000000000000000', '1000000000000000000000000')
    ).toBe(5000)
    expect(shareBps('1', '0')).toBeNull()
    expect(shareBps(null, '10')).toBeNull()
  })
})

const liquidity = (): LiquidityStatus => ({
  at: 100,
  market_hours: true,
  next_change_at: 3600,
  sender: '0xbot',
  operator_admin: '0xSafe',
  authority_target: '0xsafe',
  floor: '1000000',
  target: '5000000',
  liquid: '1000000',
  free: '10000000',
  committed: '0',
  quote_cap: null,
  step: 'raise_floor',
  message: 'Raising the floor for market hours.',
  in_flight: null,
  actions: [],
})
const liquidityView = (): ModulesView => {
  const v = view()
  v.settlement_decimals = 6
  v.config.liquidity.enabled = true
  v.status!.config.liquidity.enabled = true
  v.status!.liquidity = liquidity()
  return v
}
describe('liquidity overview state', () => {
  it('shows the live step with the bot message', () => {
    expect(liquidityState(liquidityView(), 101)).toEqual({
      label: 'Raising the floor',
      message: 'Raising the floor for market hours.',
      current: true,
    })
  })
  it('reads as a preview in shadow mode, with the target amount', () => {
    const v = liquidityView()
    v.status!.config.mode = 'shadow'
    const state = liquidityState(v, 101)
    expect(state.label).toBe('Preview · would raise the floor')
    expect(state.message).toContain('Would raise the floor to 5')
    expect(state.message).toContain('Preview sends no transactions.')
    v.status!.liquidity!.step = 'drain'
    v.status!.liquidity!.quote_cap = '2000000'
    expect(liquidityState(v, 101).message).toContain(
      'Would cap new quotes so they commit at most 2,'
    )
  })
  it('withholds stopped, missing, stale and future readings', () => {
    expect(liquidityState(liquidityView(), 145).current).toBe(true)
    expect(liquidityState(liquidityView(), 145.5).label).toBe('Reading stale')
    expect(liquidityState(liquidityView(), 99).label).toBe('Reading stale')
    const v = liquidityView()
    v.status!.liquidity = undefined
    expect(liquidityState(v, 101).label).toBe('No liquidity data')
    v.status = null
    expect(liquidityState(v, 101).label).toBe('No runtime data')
    v.running = false
    expect(liquidityState(v, 101).label).toBe('Bot stopped')
  })
  it('passes on why the bot is waiting and reports a disabled running schedule', () => {
    const v = liquidityView()
    Object.assign(v.status!.liquidity!, {
      step: 'wait',
      target: null,
      message: 'The vault is paused. Nothing is staked while it is paused',
    })
    expect(liquidityState(v, 101)).toEqual({
      label: 'Waiting',
      message: 'The vault is paused. Nothing is staked while it is paused',
      current: true,
    })
    v.status!.config.mode = 'shadow'
    expect(liquidityState(v, 101).label).toBe('Preview · waiting')
    v.status!.config.liquidity.enabled = false
    expect(liquidityState(v, 101).label).toBe('Schedule off')
  })
  it('suggests setup when the authority targets another Safe or the last action failed', () => {
    const l = liquidity()
    expect(liquiditySetupHint(l)).toBeNull()
    l.actions = [
      { at: 90, action: 'raise_floor', ok: false, detail: 'reverted' },
    ]
    expect(liquiditySetupHint(l)).toBe(
      'The last transaction failed. Grant the role to 0xbot and keep it funded with gas.'
    )
    l.actions.push({ at: 95, action: 'raise_floor', ok: true, detail: '' })
    expect(liquiditySetupHint(l)).toBeNull()
    l.authority_target = '0xother'
    expect(liquiditySetupHint(l)).toContain('Grant the role to 0xbot')
  })
})

describe('running config comparison', () => {
  it('ignores a missing liquidity key from older bots unless the schedule is on', () => {
    const v = view()
    expect(runningConfigDiffers(v)).toBe(false)
    const { liquidity: _, ...older } = v.status!.config
    v.status!.config = older as ModulesConfig
    expect(runningConfigDiffers(v)).toBe(false)
    v.config.liquidity.enabled = true
    expect(runningConfigDiffers(v)).toBe(true)
  })
  it('expects a wallet bot to run with the vault-only modules off', () => {
    const v = { ...view(), vault: false }
    v.status!.config = runnableConfig(v.config, false)
    expect(v.config.inventory.enabled).toBe(true)
    expect(runningConfigDiffers(v)).toBe(false)
    v.config = { ...v.config, curve: { ...v.config.curve, shape: 3 } }
    expect(runningConfigDiffers(v)).toBe(true)
  })
  it('still flags a vault bot running with a module the file turns on', () => {
    const v = view()
    v.status!.config = runnableConfig(v.config, false)
    expect(runningConfigDiffers(v)).toBe(true)
  })
  it('does not crash the overview on an older running config', () => {
    const v = view()
    const { liquidity: _, ...older } = v.status!.config
    v.status!.config = older as ModulesConfig
    for (const key of ['inventory', 'spreads', 'rebalance'] as const)
      v.status!.config[key].enabled = false
    expect(moduleOverviewState(v, 101).title).toBe('No modules enabled')
  })
})

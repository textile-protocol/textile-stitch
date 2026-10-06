import { describe, expect, it } from 'vitest'
import {
  equityCoordinates,
  moduleStatusFresh,
  simulationAmount,
  dynamicSpreadState,
  dynamicSpreadAdditions,
  type ModulesView,
  type ModuleStatus,
} from './modules'

describe('simulation costs and dealer depth', () => {
  it('preserves atomic precision above the safe integer range', () => {
    expect(simulationAmount('100000000000000.000000000000000001', 18)).toBe(
      '100000000000000000000000000000001'
    )
    expect(simulationAmount('0.25', 6)).toBe('250000')
    expect(simulationAmount('0', 0)).toBe('0')
  })
  it('rejects negative, ambiguous, overflowing and over-precision inputs', () => {
    for (const value of [
      '-1',
      '1,000',
      '1e6',
      '',
      '.2',
      'NaN',
      '0.0000001',
      '9'.repeat(80),
    ]) {
      expect(() => simulationAmount(value, 6)).toThrow()
    }
  })
})

describe('module telemetry', () => {
  const status = {
    at: 100,
    decisions: [{ decision: { at: 100 } }],
  } as ModuleStatus
  it('does not label old, future or stopped telemetry as current', () => {
    expect(moduleStatusFresh(status, true, 101)).toBe(true)
    expect(moduleStatusFresh(status, true, 111)).toBe(false)
    expect(moduleStatusFresh(status, true, 99)).toBe(false)
    expect(moduleStatusFresh(status, false, 101)).toBe(false)
    expect(moduleStatusFresh(null, true, 101)).toBe(false)
  })
  it('does not call recent decisions live after the runtime reports a disconnect', () => {
    const disconnected = {
      ...status,
      quote_status: {
        at: 101,
        state: 'waiting_for_session',
        message: 'Disconnected',
      },
    } as ModuleStatus
    expect(moduleStatusFresh(disconnected, true, 101)).toBe(false)
  })
  it('plots differences above Number.MAX_SAFE_INTEGER without rounding away the result', () => {
    const n = 10n ** 30n
    const points = [
      { at: 1, baseline: n.toString(), candidate: n.toString() },
      { at: 2, baseline: n.toString(), candidate: (n + 1n).toString() },
    ]
    expect(equityCoordinates(points, 'candidate')).toBe('20,180 680,30')
    expect(equityCoordinates(points, 'baseline')).toBe('20,180 680,180')
    expect(equityCoordinates([], 'baseline')).toBe('')
  })
})

describe('dynamic spread visibility', () => {
  const view = (): ModulesView => {
    const config: ModulesView['config'] = {
      mode: 'live',
      spreads: {
        enabled: true,
        max_extra_bps: 100,
        multiplier: 1,
        window_secs: 300,
        warmup_secs: 30,
      },
      inventory: {
        enabled: true,
        target_bps: 3000,
        max_bps: 6000,
        max_skew_bps: 50,
        spread_floor_bps: 5,
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
    }
    return {
      running: true,
      revision: 'test',
      settlement_decimals: 6,
      dataset_template: {},
      config,
      status: {
        version: 1,
        at: 100,
        config: structuredClone(config),
        rebalance_status: 'Disabled',
        next_attempt_at: 0,
        decisions: [
          {
            price: 1,
            settlement: '1',
            corridor: '1',
            decision: {
              version: 1,
              at: 100,
              volatility_bps: 0,
              blocked: false,
              reasons: [],
              inventory_bps: 5000,
              buy_bps: 20,
              sell_bps: 20,
              buy_limit: '1',
              rebalance_sell: '0',
            },
          },
        ],
      },
    }
  }
  it('distinguishes zero market movement from disabled or shadow behavior', () => {
    const v = view()
    expect(dynamicSpreadState(v, 101)).toMatchObject({
      label: 'Live quote policy',
      current: true,
    })
    expect(dynamicSpreadState(v, 101).message).toContain('unchanged')
    v.status!.config.spreads.multiplier = 0
    expect(dynamicSpreadState(v, 101).message).toContain('multiplier')
    v.status!.config.mode = 'shadow'
    expect(dynamicSpreadState(v, 101).label).toContain('Shadow')
    v.status!.config.spreads.enabled = false
    expect(dynamicSpreadState(v, 101).current).toBe(false)
  })
  it('withholds current values for stopped, missing or expired telemetry', () => {
    const v = view()
    expect(dynamicSpreadState(v, 111).label).toBe('Telemetry stale')
    v.status = null
    expect(dynamicSpreadState(v, 101).label).toBe('No runtime data')
    v.running = false
    expect(dynamicSpreadState(v, 101).label).toBe('Bot stopped')
  })
  it('uses current runtime mode when saved settings have not taken effect', () => {
    const v = view()
    v.config.mode = 'off'
    expect(dynamicSpreadState(v, 101).label).toBe('Live quote policy')
    v.config.spreads.enabled = false
    expect(dynamicSpreadState(v, 101).current).toBe(true)
  })
  it('shows warmup and exact runtime failure reasons instead of empty numbers', () => {
    const v = view()
    const o = v.status!.decisions[0]!
    o.decision.blocked = true
    o.decision.reasons = ['Collecting price history for dynamic spreads']
    o.inputs = {
      spread_window: {
        samples: 1,
        history_secs: 0,
        low: 1,
        high: 1,
        extra_bps: null,
      },
    } as NonNullable<typeof o.inputs>
    expect(dynamicSpreadState(v, 101).label).toBe('Warming up')
    v.status!.quote_status = {
      at: 101,
      state: 'waiting_for_price',
      message: 'Waiting for the first reference price',
    }
    expect(dynamicSpreadState(v, 101)).toMatchObject({
      current: false,
      message: 'Waiting for the first reference price',
    })
  })
  it('shows actual per-side additions instead of repeating the common buffer', () => {
    const o = view().status!.decisions[0]!
    o.decision.volatility_bps = 20
    o.decision.buy_bps = 60
    o.decision.sell_bps = 15
    o.inputs = {
      price_at: 100,
      balances_at: 100,
      base_buy_bps: 20,
      base_sell_bps: 20,
      inventory_buy_bps: 40,
      inventory_sell_bps: 5,
      spread_window: {
        samples: 3,
        history_secs: 120,
        low: 1,
        high: 1.002,
        extra_bps: 20,
      },
    }
    expect(dynamicSpreadAdditions(o)).toEqual({ buy: 20, sell: 10 })
    o.decision.buy_bps = null
    expect(dynamicSpreadAdditions(o)).toEqual({ buy: null, sell: 10 })
    o.decision.sell_bps = 5
    expect(dynamicSpreadAdditions(o)).toEqual({ buy: null, sell: 0 })
    o.inputs.inventory_buy_bps = 9995
    o.decision.buy_bps = 9999
    expect(dynamicSpreadAdditions(o).buy).toBe(4)
  })
  it('withholds additions for blocked or legacy observations without a breakdown', () => {
    const o = view().status!.decisions[0]!
    o.decision.volatility_bps = 20
    expect(dynamicSpreadAdditions(o)).toEqual({
      buy: undefined,
      sell: undefined,
    })
    o.inputs = { inventory_buy_bps: 5, inventory_sell_bps: 5 } as NonNullable<
      typeof o.inputs
    >
    o.decision.blocked = true
    expect(dynamicSpreadAdditions(o)).toEqual({
      buy: undefined,
      sell: undefined,
    })
    expect(dynamicSpreadAdditions(undefined)).toEqual({
      buy: undefined,
      sell: undefined,
    })
  })
  it('explains a zero sell buffer at the inventory ceiling even when prices moved', () => {
    const v = view()
    v.status!.config.spreads.inventory_aware = true
    const o = v.status!.decisions[0]!
    o.decision.volatility_bps = 20
    o.decision.inventory_bps = 9945
    o.decision.buy_bps = null
    o.decision.sell_bps = 5
    o.inputs = {
      inventory_buy_bps: null,
      inventory_sell_bps: 5,
    } as NonNullable<typeof o.inputs>
    expect(dynamicSpreadState(v, 101).message).toContain(
      'Inventory weighting removes'
    )
    o.decision.inventory_bps = v.status!.config.inventory.target_bps
    o.decision.buy_bps = 9999
    o.decision.sell_bps = null
    o.inputs.inventory_buy_bps = 9999
    expect(dynamicSpreadState(v, 101).message).not.toContain(
      'Inventory weighting removes'
    )
    v.status!.config.mode = 'shadow'
    expect(dynamicSpreadState(v, 101).message).toContain(
      'leaves live quotes unchanged'
    )
  })
})

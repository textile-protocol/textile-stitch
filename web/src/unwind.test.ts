import { describe, expect, it } from 'vitest'
import {
  defaultCurve,
  defaultProfitUnwind,
  draftUnwindCapLimit,
  leanConflict,
} from './modulePresentation'
import type { ModulesView } from './modules'
import {
  averageUnwindWeight,
  flatOrderNgn,
  flatUnwindSide,
  liftedFlatOrderNgn,
  minSpreadBps,
  profitUnwindErrors,
  unwindCapHint,
  unwindFullNgn,
  unwindOverviewState,
  unwindRequirements,
  unwindWeight,
  type UnwindReading,
} from './unwind'

describe('profit unwind', () => {
  const knobs = {
    share: 0.5,
    cap_ngn: 2,
    min_gain_ngn: 0.7,
    start_pct: 0,
  }
  const at = (cost: number, mid = 1380) => ({ cost, feed_mid: mid })

  it('matches the spec worked example and the bot', () => {
    // Taken in at 1386, feed 1380: the 2 NGN cap binds, and at 75% naira
    // three quarters of it applies, about 1.5 NGN (pricing/unwind.rs pins
    // the same).
    const full = unwindFullNgn(knobs, at(1386))
    expect(Math.abs(full - 2)).toBeLessThan(1e-9)
    const lift = (start: number) => full * unwindWeight(0.75, start)
    expect(Math.abs(lift(0) - 1.5)).toBeLessThan(1e-9)
    expect(Math.abs(lift(50) - 1)).toBeLessThan(1e-9)
  })

  it('moves nothing on a small gain or a loss', () => {
    expect(unwindFullNgn(knobs, at(1380.5))).toBe(0)
    expect(unwindFullNgn(knobs, at(1374))).toBe(0)
  })

  it('starts at the start share and rises evenly', () => {
    expect(unwindWeight(0.5, 50)).toBe(0)
    expect(unwindWeight(0.75, 50)).toBeCloseTo(0.5, 12)
    expect(unwindWeight(1, 50)).toBe(1)
    // The stretch average is order-free and the point weight at zero width.
    expect(averageUnwindWeight(0.6, 0.9, 50)).toBe(
      averageUnwindWeight(0.9, 0.6, 50)
    )
    expect(averageUnwindWeight(0.8, 0.8, 50)).toBe(unwindWeight(0.8, 50))
    expect(averageUnwindWeight(0.1, 0.4, 50)).toBe(0)
  })

  it('lets a cap above the round-trip limit save', () => {
    // The limit is advisory: the cap field warns, the bot quotes it as set.
    const base = { ...knobs, enabled: true, live: false, refresh_secs: 900 }
    expect(profitUnwindErrors({ ...base, cap_ngn: 4.6 })).toEqual([])
  })

  it("warns about a cap above the limit, and when it can't size it", () => {
    const hint = (cap: number, limit: number | null) =>
      unwindCapHint(cap, limit, 'USDT', (n) => n.toFixed(2))
    const risk = 'a taker can profit from the lift falling between trades'
    expect(hint(2, 1.1)).toContain(
      `Above 1.10 at today's spread and start, ${risk}`
    )
    expect(hint(1, 1.1)).toContain(
      "Up to 1.10 at today's spread and start, a taker can't"
    )
    // No fresh reading (first enable, or stale): Save lets any cap through,
    // so the hint still names the risk.
    expect(hint(2, null)).toContain(risk)
    expect(hint(2, null)).toContain("can't size the round-trip limit")
  })

  it('runs without the curve and checks its knobs', () => {
    const base = { ...knobs, enabled: true, live: false, refresh_secs: 900 }
    expect(profitUnwindErrors(base)).toEqual([])
    expect(profitUnwindErrors({ ...base, share: 2 })).toHaveLength(1)
    expect(profitUnwindErrors({ ...base, start_pct: 100 })).toHaveLength(1)
  })

  it('lifts a flat order by the averaged lift and floors the ask at the cost', () => {
    // 1.38 NGN on a 1380 centre is 10 bps; from no start, the top half of
    // the share averages three quarters of it (pricing/unwind.rs pins the
    // same).
    const lift = (costNgn: number) => ({ fullNgn: 1.38, startPct: 0, costNgn })
    const at = (order: number, buys: boolean, cost: number) =>
      liftedFlatOrderNgn(order, buys, lift(cost), 1380, 0.5, 1)
    expect(at(1382, true, 2000)).toBeCloseTo(1382 * 1.00075, 9)
    expect(at(1379, false, 2000)).toBeCloseTo(1379 * 1.00075, 9)
    expect(at(1379, false, 1379.5)).toBeCloseTo(1379.5, 9)
    expect(at(1379, false, 1378)).toBe(1379)
    expect(
      liftedFlatOrderNgn(1379, false, lift(2000), 1380, 0.1, 0.4)
    ).toBeCloseTo(1379 * 1.00025, 9)
  })

  it('holds a flat lift to the smaller bps spread', () => {
    const spreads = (buy: number | null, sell: number | null) => ({
      corridor_spreads: {
        buy: buy === null ? null : { kind: 'bps' as const, value: buy },
        sell: sell === null ? null : { kind: 'bps' as const, value: sell },
      },
    })
    expect(minSpreadBps(spreads(30, 50))).toBe(30)
    expect(minSpreadBps(spreads(null, 50))).toBe(50)
    expect(minSpreadBps(spreads(null, null))).toBe(null)
    expect(
      minSpreadBps({
        corridor_spreads: {
          buy: { kind: 'abs', value: 0.001 },
          sell: { kind: 'bps', value: 50 },
        },
      })
    ).toBe(null)
  })
})

describe("profit unwind's Simulate example on a flat price", () => {
  const FEED = 1380
  const lift = { fullNgn: 1.38, startPct: 0, costNgn: 2000 }
  const side = (over: Partial<Parameters<typeof flatUnwindSide>[0]>) => {
    const botBuys = over.botBuys ?? false
    const order = flatOrderNgn(FEED, 5, botBuys)
    return flatUnwindSide({
      base: order * (botBuys ? 1.0001 : 0.9999),
      order,
      lift,
      live: true,
      midNgn: FEED,
      share: 0.5,
      after: 1,
      botBuys,
      ...over,
    })
  }

  it('reads the flat order the way the bot builds it', () => {
    // bid_price is mid * (1 - spread) in the bot's units, ask_price
    // mid * (1 + spread): in NGN per USDT the bid sits above the feed.
    expect(flatOrderNgn(FEED, 5, true)).toBeCloseTo(FEED / 0.9995, 9)
    expect(flatOrderNgn(FEED, 5, false)).toBeCloseTo(FEED / 1.0005, 9)
  })

  it('lifts both sides by the averaged lift, and only a live one goes out', () => {
    // 10 bps at full; the top half of the share averages three quarters.
    for (const botBuys of [true, false]) {
      const s = side({ botBuys })
      expect(s.lift!).toBeCloseTo(7.5, 6)
      expect(s.price).toBe(s.unwound)
      const preview = side({ botBuys, live: false })
      expect(preview.lift).toBe(s.lift)
      expect(preview.price).toBe(
        flatOrderNgn(FEED, 5, botBuys) * (botBuys ? 1.0001 : 0.9999)
      )
      expect(s.atCost).toBe(false)
    }
  })

  it('marks a sell the cost floor holds back', () => {
    const order = flatOrderNgn(FEED, 5, false)
    const tight = { ...lift, costNgn: order + 0.3 }
    const s = side({ lift: tight })
    expect(s.atCost).toBe(true)
    expect(s.lift!).toBeLessThan(7.5)
    expect(side({ lift: tight, botBuys: true }).atCost).toBe(false)
  })

  it('shows no lift without one', () => {
    const s = side({ lift: null })
    expect([s.unwound, s.lift, s.atCost]).toEqual([null, null, false])
  })
})

describe('unwindOverviewState', () => {
  const reading = (gain: number, full: number): UnwindReading => ({
    at: 100,
    share: 0.8,
    value: 300_000,
    fee_bps: 1,
    cost: 1380 + gain,
    snapshot_at: 90,
    feed_mid: 1380,
    gain,
    full_shift: full,
    live: false,
  })
  const view = (
    over: Partial<ModulesView['config']['profit_unwind']>,
    mode: 'off' | 'shadow' | 'live',
    r: UnwindReading | null
  ) => ({
    running: true,
    config: {
      mode,
      curve: defaultCurve(),
      profit_unwind: { ...defaultProfitUnwind(), enabled: true, ...over },
    } as ModulesView['config'],
    status: r
      ? ({
          profit_unwind: { reading: r, quotes: [] },
        } as unknown as ModulesView['status'])
      : null,
  })

  it('names what profit unwind is doing, on its own switch and the mode', () => {
    expect(
      unwindOverviewState(view({ enabled: false }, 'live', null), 100).title
    ).toBe('Profit unwind is off')
    expect(unwindOverviewState(view({}, 'live', null), 100).title).toBe(
      'Waiting for a reading'
    )
    expect(
      unwindOverviewState(view({}, 'live', reading(0.2, 0)), 100).title
    ).toBe('Nothing to give back')
    // Its own switch and Live mode, both, before it quotes.
    const lifting = reading(6, 2)
    expect(
      unwindOverviewState(view({ live: true }, 'live', lifting), 100).pill
    ).toBe('Live')
    expect(
      unwindOverviewState(view({ live: true }, 'shadow', lifting), 100).pill
    ).toBe('Preview')
    expect(
      unwindOverviewState(view({ live: false }, 'live', lifting), 100).pill
    ).toBe('Preview')
    // An old reading isn't "now".
    expect(
      unwindOverviewState(view({ live: true }, 'live', lifting), 200).title
    ).toBe('Waiting for a reading')
  })
})

describe('profit unwind beside other modules and saves', () => {
  const reading: UnwindReading = {
    at: 100,
    share: 0.8,
    value: 300_000,
    fee_bps: 1,
    cost: 1386,
    snapshot_at: 90,
    feed_mid: 1380,
    gain: 6,
    full_shift: 2,
    live: false,
  }
  const config = (over: Record<string, unknown> = {}) =>
    ({
      mode: 'live',
      inventory: { enabled: true },
      curve: defaultCurve(),
      profit_unwind: { ...defaultProfitUnwind(), enabled: true },
      ...over,
    }) as unknown as ModulesView['config']
  const view = (vault: boolean) =>
    ({
      config: config(),
      vault,
      running: true,
      feed_inverted: false,
      corridor_spreads: {
        buy: { kind: 'bps', value: 1 },
        sell: { kind: 'bps', value: 1 },
      },
      status: { profit_unwind: { reading, quotes: [] } },
    }) as unknown as ModulesView

  it('runs beside inventory balancing only on a wallet', () => {
    const c = config()
    expect(leanConflict(c, 'profit_unwind', true)).toMatch(/turn it off/)
    expect(leanConflict(c, 'profit_unwind', false)).toBeNull()
    const vaultCheck = unwindRequirements(view(true), 'cNGN', 'USDT', 100)
    expect(
      vaultCheck.find((r) => r.label === 'Inventory balancing off')?.ok
    ).toBe(false)
    expect(
      unwindRequirements(view(false), 'cNGN', 'USDT', 100).some(
        (r) => r.label === 'Inventory balancing off'
      )
    ).toBe(false)
  })

  it('sizes the cap limit off the lower of the feed and the curve mid', () => {
    // 4 x 1 bps x mid / (1 - 0.5): about 1.10 NGN at the 1380 feed.
    const limit = (mid: number) => (4 * 1e-4 * mid) / 0.5
    const flat = config()
    expect(draftUnwindCapLimit(view(true), flat, 100)).toBeCloseTo(
      limit(1380),
      9
    )
    // With the curve on the bot may quote off its reference mid, up to 1%
    // under the feed: the limit there is lower, so that's the one to show.
    const onCurve = config({ curve: { ...defaultCurve(), enabled: true } })
    const withCurve = (referenceMid: number) =>
      ({
        ...view(true),
        status: {
          profit_unwind: { reading, quotes: [] },
          curve: { reading: { at: 100, reference_mid: referenceMid } },
        },
      }) as unknown as ModulesView
    expect(draftUnwindCapLimit(withCurve(1370), onCurve, 100)).toBeCloseTo(
      limit(1370),
      9
    )
    expect(draftUnwindCapLimit(withCurve(1390), onCurve, 100)).toBeCloseTo(
      limit(1380),
      9
    )
    // No fresh curve reading: the curve's limit is unknown, so no limit.
    expect(draftUnwindCapLimit(view(true), onCurve, 100)).toBeNull()
  })
})

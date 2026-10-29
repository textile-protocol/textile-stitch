import { describe, expect, it } from 'vitest'
import {
  askFloorBinds,
  averageShade,
  bpsOver,
  cheapestUntil,
  curveConfigErrors,
  curveOverviewState,
  curveFor,
  DEFAULT_SHAPE,
  exampleSide,
  feedPrice,
  flatPrice,
  knobLabel,
  MAX_REFERENCE_GAP_BPS,
  readsUnadjustedPrice,
  referenceTooFar,
  shadeAt,
  shareAfter,
  shapeLabel,
  takerPrice,
  withLift,
} from './curve'
import { liftLimitNgn, unwindWeight } from './unwind'

// The fixtures and golden values of src/pricing/curve.rs, so the Simulate
// drawer prices exactly what the bot does.
const MID = 1380.78
const HALF = 9.4
const ALL_IN = 2
const reference = { mid: MID, half_range_bps: HALF }
const knobs = (start: number, end: number) => ({
  start_below_mid: start,
  end_above_buy: end,
  shape: DEFAULT_SHAPE,
})
const knobs0 = () => knobs(0, 0)
const close = (got: number, want: number) =>
  expect(Math.abs((got - want) / want)).toBeLessThan(1e-9)

describe('inventory curve', () => {
  it('matches the bot to nine decimals', () => {
    const zero = curveFor(reference, ALL_IN, knobs(0, 0))!
    close(zero.s0, -1.9996000799837876)
    close(zero.s1, 11.402280456089819)
    close(averageShade(zero, 0.3, 0.55), 0.49091605296989105)
    const wide = curveFor(reference, ALL_IN, knobs(0.5, 1))!
    close(shadeAt(wide, 0.9), 14.035466807876642)
  })

  it('lands both ends on the knobs, all-in', () => {
    const settings: [number, number][] = [
      [0, 0],
      [0.5, 1],
      [1.25, 0.25],
      [-0.5, -0.2],
      [-0.9, 0.3],
    ]
    for (const [start, end] of settings) {
      const c = curveFor(reference, ALL_IN, knobs(start, end))!
      close(takerPrice(c, true, 0, 0), MID - start)
      close(takerPrice(c, false, 1, 1), MID * (1 + HALF / 10_000) + end)
    }
  })

  it('never pays a round trip, narrowed or crossed', () => {
    for (const [start, end] of [
      [0.5, 1],
      [-0.5, -0.3],
      [-1, -1],
    ]) {
      const c = curveFor(reference, ALL_IN, knobs(start!, end!))!
      for (let f0 = 0; f0 <= 1; f0 += 0.05) {
        for (let t = 0.01; f0 + t <= 1; t += 0.07) {
          // Bot buys cNGN out to f0 + t, then sells it straight back.
          const out = takerPrice(c, true, f0, f0 + t)
          const back = takerPrice(c, false, f0 + t, f0)
          expect(back).toBeLessThan(out)
        }
      }
    }
  })

  it('narrows on negative knobs and goes flat, not backwards, when they cross', () => {
    const wide = curveFor(reference, ALL_IN, knobs(0, 0))!
    const narrow = curveFor(reference, ALL_IN, knobs(-0.5, -0.3))!
    expect(narrow.s0).toBeGreaterThan(wide.s0)
    expect(narrow.s1).toBeLessThan(wide.s1)
    expect(takerPrice(narrow, true, 0, 0)).toBeGreaterThan(MID)
    // The buy sits about 1.3 NGN over the mid and the round trip costs about
    // 0.55: the shades cross past about 1.85 off, so 2 off goes flat.
    const crossed = curveFor(reference, ALL_IN, knobs(-1, -1))!
    expect(crossed.s0).toBe(crossed.s1)
    expect(shadeAt(crossed, 0)).toBe(shadeAt(crossed, 1))
    const until = cheapestUntil(crossed, HALF)
    expect(until === 0 || until === 1).toBe(true)
    // Walking the end knob through the crossing never jumps the price.
    let last: number | null = null
    for (let end = 0; end > -2.5; end -= 0.01) {
      const p = takerPrice(
        curveFor(reference, ALL_IN, knobs(-1, end))!,
        true,
        0.5,
        0.5
      )
      if (last !== null)
        expect(Math.abs((p / last - 1) * 10_000)).toBeLessThan(0.5)
      last = p
    }
  })

  it('crosses the all-in ends before the shades', () => {
    // The all-in sell at no naira sits above the all-in buy full of it, but
    // the shades still rise: still a curve.
    const c = curveFor(reference, ALL_IN, knobs(-0.7, -0.7))!
    expect(takerPrice(c, true, 0, 0)).toBeGreaterThan(
      takerPrice(c, false, 1, 1)
    )
    expect(c.s1).toBeGreaterThan(c.s0)
    // The shades cross at about 0.925 off each end.
    const just = curveFor(reference, ALL_IN, knobs(-0.92, -0.92))!
    expect(just.s1).toBeGreaterThan(just.s0)
    const past = curveFor(reference, ALL_IN, knobs(-0.93, -0.93))!
    expect(past.s1).toBe(past.s0)
  })

  it('stays the cheapest dollars past 80% naira at zero knobs', () => {
    const c = curveFor(reference, ALL_IN, knobs(0, 0))!
    const f = cheapestUntil(c, HALF)
    expect(f).toBeGreaterThan(0.8)
    expect(f).toBeLessThan(0.88)
  })

  it('makes no curve from a crossed or missing reference', () => {
    expect(
      curveFor({ mid: MID, half_range_bps: 0 }, ALL_IN, knobs(0, 0))
    ).toBeNull()
    expect(
      curveFor({ mid: 0, half_range_bps: HALF }, ALL_IN, knobs(0, 0))
    ).toBeNull()
    expect(curveFor(reference, ALL_IN, knobs(MID, 0))).toBeNull()
    expect(curveFor(reference, ALL_IN, knobs(0, -2 * MID))).toBeNull()
  })

  it('names its knobs the way the row shows them', () => {
    expect(knobLabel('Mid', 0)).toBe('Mid')
    expect(knobLabel('Mid', 0.5)).toBe('Mid − 0.5')
    expect(knobLabel('Buy', 1)).toBe('Buy + 1')
    expect(knobLabel('Mid', -0.5)).toBe('Mid + 0.5')
    expect(knobLabel('Buy', -0.3)).toBe('Buy − 0.3')
    expect(shapeLabel(2)).toBe('f²')
    expect(shapeLabel(2.5)).toBe('f^2.5')
  })

  it('takes negative knobs, refuses a non-number, an offset past the controls and a shape out of range', () => {
    expect(curveConfigErrors(knobs(0, 0))).toEqual([])
    expect(curveConfigErrors(knobs(-1, -0.5))).toEqual([])
    expect(curveConfigErrors(knobs(-1000, 1000))).toEqual([])
    expect(curveConfigErrors(knobs(NaN, 0))).toHaveLength(1)
    expect(curveConfigErrors(knobs(-1001, 0))).toHaveLength(1)
    expect(curveConfigErrors(knobs(0, -1001))).toHaveLength(1)
    expect(curveConfigErrors(knobs(1001, 0))).toHaveLength(1)
    expect(
      curveConfigErrors({ start_below_mid: 0, end_above_buy: 0, shape: 9 })
    ).toHaveLength(1)
  })
})

describe('flat quote', () => {
  // The feed price the bot quotes flat around: on cNGN a few bps above the
  // reference mid.
  const FEED = 1381.4
  const reading = {
    at: 1,
    share: 0.5,
    value: 1,
    reference_mid: MID,
    half_range_bps: HALF,
    fee_bps: 1,
  }

  it('sits around the feed price, not the reference mid', () => {
    close(flatPrice(FEED, ALL_IN, true), FEED * (1 + ALL_IN / 10_000))
    close(flatPrice(FEED, ALL_IN, false), FEED * (1 - ALL_IN / 10_000))
  })

  it('leans by the curve against the feed price, whatever the spread', () => {
    for (const allIn of [0, ALL_IN, 25]) {
      const c = curveFor(reference, allIn, knobs(1, 0.5))!
      for (const botBuys of [true, false]) {
        const lean = bpsOver(
          takerPrice(c, botBuys, 0.5, 0.6),
          flatPrice(FEED, allIn, botBuys)
        )
        const shaded = MID * (1 + averageShade(c, 0.5, 0.6) / 10_000)
        expect(lean).toBeCloseTo(bpsOver(shaded, FEED), 9)
      }
    }
  })

  it('reads the feed price off the reading, and none off an older bot', () => {
    expect(feedPrice({ ...reading, feed_price: FEED })).toBe(FEED)
    for (const feed_price of [undefined, null, 0, -1, NaN, Infinity])
      expect(feedPrice({ ...reading, feed_price })).toBeNull()
  })

  it('reads the unadjusted feed price only while the curve runs Live', () => {
    const config = (mode: 'off' | 'shadow' | 'live', enabled: boolean) => ({
      mode,
      curve: { enabled, ...knobs(0, 0) },
    })
    expect(readsUnadjustedPrice(config('live', true))).toBe(true)
    expect(readsUnadjustedPrice(config('live', false))).toBe(false)
    expect(readsUnadjustedPrice(config('shadow', true))).toBe(false)
    expect(readsUnadjustedPrice(config('off', true))).toBe(false)
  })

  it('goes flat, as the bot does, once the reference is 1% from the feed', () => {
    const buy = MID * (1 + HALF / 10_000)
    const at = (from: number, bps: number) => from * (1 + bps / 10_000)
    const tooFar = (feed: number) => referenceTooFar(reading, feed)
    expect(tooFar(FEED)).toBe(false)
    // Both ends must be within 1%: below the book the buy is the far end,
    // above it the mid is.
    expect(tooFar(at(buy, -MAX_REFERENCE_GAP_BPS + 0.1))).toBe(false)
    expect(tooFar(at(buy, -MAX_REFERENCE_GAP_BPS - 0.1))).toBe(true)
    expect(tooFar(at(MID, MAX_REFERENCE_GAP_BPS - 0.1))).toBe(false)
    expect(tooFar(at(MID, MAX_REFERENCE_GAP_BPS + 0.1))).toBe(true)
  })
})

describe('profit unwind on the curve', () => {
  it('lifts both sides by the weighted shade and floors the ask at the cost', () => {
    const c = curveFor(reference, ALL_IN, knobs0())!
    const up = withLift(c, { fullNgn: 1.5, startPct: 50, costNgn: MID * 2 })
    for (const f of [0, 0.5, 0.75, 1])
      expect(shadeAt(up, f) - shadeAt(c, f)).toBeCloseTo(
        (1.5 / MID) * 10_000 * unwindWeight(f, 50),
        12
      )
    // NGN off the curve's own mid: 1% away from the feed, still 1 NGN
    // (before the bid's all-in, which scales it).
    const wide = curveFor({ ...reference, mid: MID * 1.01 }, ALL_IN, knobs0())!
    const wideUp = withLift(wide, { fullNgn: 1, startPct: 0, costNgn: MID * 2 })
    expect(
      takerPrice(wideUp, true, 1, 1) - takerPrice(wide, true, 1, 1)
    ).toBeCloseTo(1 + ALL_IN / 10_000, 9)
    // A cost just over the plain ask stops the lifted ask there.
    const plainAsk = takerPrice(c, false, 1, 0.98)
    const tight = withLift(c, {
      fullNgn: 2,
      startPct: 0,
      costNgn: plainAsk + 0.3,
    })
    expect(takerPrice(tight, false, 1, 0.98)).toBeCloseTo(plainAsk + 0.3, 9)
    expect(takerPrice(tight, true, 0.98, 1)).toBeGreaterThan(
      takerPrice(c, true, 0.98, 1)
    )
    // With a venue fee the bot floors the signed order (spread only) and the
    // fee comes off after, so the taker realizes the cost less the fee.
    const feed = curveFor(reference, ALL_IN, knobs0(), ALL_IN - 1)!
    const cost = takerPrice(feed, false, 1, 0.98) + 0.5
    const fee = withLift(feed, { fullNgn: 2, startPct: 0, costNgn: cost })
    const spread = (ALL_IN - 1) / 10_000
    expect(takerPrice(fee, false, 1, 0.98)).toBeCloseTo(
      (cost * (1 - ALL_IN / 10_000)) / (1 - spread),
      9
    )
    // Nothing to trust, nothing lifted.
    expect(withLift(c, { fullNgn: 0, startPct: 0, costNgn: MID }).lift).toBe(
      undefined
    )
  })

  it('holds the lift to the round-trip limit, as the bot does', () => {
    // 4 x spread x mid / (1 - start): about 0.55 NGN at a 1 bps spread and
    // start 0. The fee isn't in it: it arrives per request.
    const limit = liftLimitNgn(ALL_IN - 1, MID, 0)
    expect(limit).toBeCloseTo((4 * (ALL_IN - 1) * MID) / 10_000, 9)
    expect(liftLimitNgn(ALL_IN - 1, MID, 50)).toBeCloseTo(2 * limit, 9)
    const c = curveFor(reference, ALL_IN, knobs0(), ALL_IN - 1)!
    const big = withLift(c, { fullNgn: 10, startPct: 0, costNgn: MID * 2 })
    expect(big.lift?.fullNgn).toBeCloseTo(limit, 9)
    // A NaN lift stays off rather than becoming the limit.
    expect(
      withLift(c, { fullNgn: NaN, startPct: 0, costNgn: MID * 2 }).lift
    ).toBe(undefined)
  })
})

describe('the Simulate example with profit unwind', () => {
  const FEED = 1381.4
  const plain = curveFor(reference, ALL_IN, knobs0(), ALL_IN - 1)!
  const lifted = withLift(plain, { fullNgn: 1, startPct: 50, costNgn: 1400 })
  const side = (over: Partial<Parameters<typeof exampleSide>[0]>) =>
    exampleSide({
      plain,
      lifted,
      unwindLive: true,
      feed: FEED,
      flatOnly: false,
      share: 0.82,
      sizeUsd: 25_000,
      valueUsd: 300_000,
      botBuys: false,
      ...over,
    })

  it('splits the move off flat into the curve and profit unwind', () => {
    for (const botBuys of [true, false]) {
      const s = side({ botBuys })
      expect(s.unwind!).toBeGreaterThan(0)
      // The two rows add up to the quote's move off flat.
      expect(s.lean! + s.unwind!).toBeCloseTo(bpsOver(s.price, s.flat!), 2)
      // And the plain curve's lean is what the bot would quote without it.
      const bare = side({ botBuys, lifted: null })
      expect(bare.unwind).toBeNull()
      expect(s.lean).toBe(bare.lean)
      expect(s.curved).toBe(bare.price)
    }
  })

  it('leaves a previewed lift out of the quote but still shows it', () => {
    for (const botBuys of [true, false]) {
      const live = side({ botBuys })
      const preview = side({ botBuys, unwindLive: false })
      expect(preview.price).toBe(side({ botBuys, lifted: null }).price)
      expect(preview.unwind).toBe(live.unwind)
    }
  })

  it('shows no curve lean while the bot quotes flat, only a flat lift', () => {
    const s = side({ flatOnly: true })
    expect(s.price).toBe(s.flat)
    expect([s.lean, s.unwind]).toEqual([null, null])
    // Quoting flat, a live lift goes on the flat price, as the bot does.
    const lifted = side({
      flatOnly: true,
      flatUnwind: { unwound: 1400, price: 1400, lift: 6, atCost: false },
    })
    expect([lifted.price, lifted.lean, lifted.unwind]).toEqual([1400, null, 6])
  })

  it('marks a sell the cost floor holds back, and never a buy', () => {
    // A cost just over the plain sell's order price: the lift reaches it.
    const after = shareAfter(0.95, 25_000, 300_000, false)
    const order = (c: typeof plain) =>
      (takerPrice(c, false, 0.95, after) * (1 - (ALL_IN - 1) / 10_000)) /
      (1 - ALL_IN / 10_000)
    const costNgn = order(plain) + 0.2
    const floored = withLift(plain, { fullNgn: 1, startPct: 0, costNgn })
    expect(askFloorBinds(floored, 0.95, after)).toBe(true)
    expect(side({ lifted: floored, share: 0.95 }).atCost).toBe(true)
    expect(side({ lifted: floored, share: 0.95, botBuys: true }).atCost).toBe(
      false
    )
    // Far under the cost, or with no lift, nothing is held back.
    expect(side({ share: 0.95 }).atCost).toBe(false)
    expect(askFloorBinds(plain, 0.95, after)).toBe(false)
  })
})

describe('curveOverviewState', () => {
  const reading = {
    at: 1000,
    share: 0.8,
    value: 300_000,
    reference_mid: MID,
    half_range_bps: HALF,
    fee_bps: 1,
  }
  const view = (
    mode: 'off' | 'shadow' | 'live',
    running = true,
    at = 1000
  ) => ({
    running,
    config: { mode, curve: { enabled: true, ...knobs(0, 0) } },
    status: { curve: { reading: { ...reading, at }, quotes: [] } },
  })

  it('names what the bot is doing with the curve', () => {
    expect(curveOverviewState(view('shadow') as never, 1005).title).toBe(
      'Previewing the curve'
    )
    expect(curveOverviewState(view('live') as never, 1005).title).toBe(
      'Quoting off the curve'
    )
    expect(curveOverviewState(view('off') as never, 1005).title).toBe(
      'The curve is off'
    )
    expect(curveOverviewState(view('live', false) as never, 1005).pill).toBe(
      'Bot stopped'
    )
  })

  it('does not call an old reading "now"', () => {
    expect(
      curveOverviewState(view('live', true, 900) as never, 1005).title
    ).toBe('Waiting for a reading')
  })
})

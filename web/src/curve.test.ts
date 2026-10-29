import { describe, expect, it } from 'vitest'
import {
  averageShade,
  bpsOver,
  cheapestUntil,
  curveConfigErrors,
  curveOverviewState,
  curveFor,
  DEFAULT_SHAPE,
  feedPrice,
  flatPrice,
  knobLabel,
  MAX_REFERENCE_GAP_BPS,
  readsUnadjustedPrice,
  referenceTooFar,
  shadeAt,
  shapeLabel,
  takerPrice,
} from './curve'

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

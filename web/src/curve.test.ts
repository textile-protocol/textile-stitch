import { describe, expect, it } from 'vitest'
import {
  averageShade,
  cheapestUntil,
  curveConfigErrors,
  curveOverviewState,
  curveFor,
  DEFAULT_SHAPE,
  knobLabel,
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
    ]
    for (const [start, end] of settings) {
      const c = curveFor(reference, ALL_IN, knobs(start, end))!
      close(takerPrice(c, true, 0, 0), MID - start)
      close(takerPrice(c, false, 1, 1), MID * (1 + HALF / 10_000) + end)
    }
  })

  it('never pays a round trip', () => {
    const c = curveFor(reference, ALL_IN, knobs(0.5, 1))!
    for (let f0 = 0; f0 <= 1; f0 += 0.05) {
      for (let t = 0.01; f0 + t <= 1; t += 0.07) {
        // Bot buys cNGN out to f0 + t, then sells it straight back.
        const out = takerPrice(c, true, f0, f0 + t)
        const back = takerPrice(c, false, f0 + t, f0)
        expect(back).toBeLessThan(out)
      }
    }
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
  })

  it('names its knobs the way the row shows them', () => {
    expect(knobLabel('Mid', 0)).toBe('Mid')
    expect(knobLabel('Mid', 0.5)).toBe('Mid − 0.5')
    expect(knobLabel('Buy', 1)).toBe('Buy + 1')
    expect(shapeLabel(2)).toBe('f²')
    expect(shapeLabel(2.5)).toBe('f^2.5')
  })

  it('refuses negative knobs and a shape out of range', () => {
    expect(curveConfigErrors(knobs(0, 0))).toEqual([])
    expect(curveConfigErrors(knobs(-1, 0))).toHaveLength(1)
    expect(
      curveConfigErrors({ start_below_mid: 0, end_above_buy: 0, shape: 9 })
    ).toHaveLength(1)
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

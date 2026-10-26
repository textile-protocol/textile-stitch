// The inventory curve as the panel sees it: the shapes the curve API returns,
// and the curve maths for the Simulate drawer. The maths mirrors
// `src/pricing/curve.rs` and is pinned to the same golden values in
// curve.test.ts, so the example and the bot can't drift apart unnoticed.
//
// Units: everything here is in soft currency per dollar (NGN per USDT), the
// unit the operator, the reference and both knobs live in. The bot's own order
// amounts are the reciprocal; the panel never needs them.

import type { CurveModuleConfig, ModulesView } from './modules'

/** One level tick's view of the curve's inputs, as the bot records it in the
 * module status. Prices are NGN per USDT. Mirrors `CurveReading` in
 * src/modules/runtime.rs. */
export interface CurveReading {
  at: number
  /** The corridor currency's share of what the bot holds, 0..1, before this
   * side's open quotes. */
  share: number
  /** What the bot holds, valued in the settlement currency. */
  value: number
  reference_mid: number
  half_range_bps: number
  /** The venue fee on the last quote request, bps. */
  fee_bps: number
}

/** One Swap request the curve priced, or answered flat. */
export interface CurveQuote {
  at: number
  bot_buys: boolean
  live: boolean
  /** Settlement units. */
  size: number
  share_before: number | null
  share_after: number | null
  /** Against the flat quote, bps of NGN per USDT; null when it went flat. */
  lean_bps: number | null
  flat_reason: string | null
}

export interface CurveStatus {
  reading: CurveReading | null
  /** Oldest first, up to 200. */
  quotes: CurveQuote[]
}

/** The reference book in NGN per USDT, as the maths below reads it. */
export interface CurveReference {
  mid: number
  half_range_bps: number
}

type Knobs = Pick<
  CurveModuleConfig,
  'start_below_mid' | 'end_above_buy' | 'shape'
>

export const DEFAULT_SHAPE = 2

/** A curve sized for one reference book and one all-in cost. */
export interface Curve {
  mid: number
  /** Shade at no naira, bps off the mid. Negative at zero knobs. */
  s0: number
  /** Shade at all naira, bps off the mid. */
  s1: number
  shape: number
  allInBps: number
}

/** The curve for this reference and cost, or null when the inputs make none
 * (the bot then quotes flat). `allInBps` is the corridor spread plus the venue
 * fee: the endpoints are the prices a taker actually pays. */
export function curveFor(
  reference: CurveReference,
  allInBps: number,
  config: Knobs
): Curve | null {
  const { mid, half_range_bps: half } = reference
  const { start_below_mid: start, end_above_buy: end, shape } = config
  const finite = [mid, half, allInBps, start, end, shape].every(Number.isFinite)
  if (!finite || mid <= 0 || half <= 0 || shape <= 0) return null
  if (allInBps < 0 || allInBps >= 10_000 || start < 0 || end < 0) return null
  if (mid - start <= 0) return null
  const buy = mid * (1 + half / 10_000)
  const s0 = ((mid - start) / (mid * (1 + allInBps / 10_000)) - 1) * 10_000
  const s1 = ((buy + end) / (mid * (1 - allInBps / 10_000)) - 1) * 10_000
  return s1 > s0 ? { mid, s0, s1, shape, allInBps } : null
}

const clamp01 = (f: number) => Math.min(1, Math.max(0, f))

/** Shade at one naira share, bps off the mid. */
export function shadeAt(curve: Curve, f: number): number {
  return curve.s0 + (curve.s1 - curve.s0) * clamp01(f) ** curve.shape
}

/** The shade a quote pays: the mean over the stretch of share it moves
 * through, in either direction. A zero-width stretch is the point shade. */
export function averageShade(curve: Curve, f0: number, f1: number): number {
  const lo = Math.min(clamp01(f0), clamp01(f1))
  const hi = Math.max(clamp01(f0), clamp01(f1))
  if (hi - lo <= Number.EPSILON) return shadeAt(curve, lo)
  const p = curve.shape + 1
  return (
    curve.s0 + ((curve.s1 - curve.s0) * (hi ** p - lo ** p)) / (p * (hi - lo))
  )
}

/** The all-in price a taker gets for the stretch, NGN per USDT: what the bot
 * pays in naira per dollar when it buys cNGN, or takes when it sells. */
export function takerPrice(
  curve: Curve,
  botBuys: boolean,
  f0: number,
  f1: number
): number {
  const centre = curve.mid * (1 + averageShade(curve, f0, f1) / 10_000)
  const half = curve.allInBps / 10_000
  return botBuys ? centre * (1 + half) : centre * (1 - half)
}

/** The flat all-in price at the reference mid, for comparison. */
export function flatPrice(curve: Curve, botBuys: boolean): number {
  const half = curve.allInBps / 10_000
  return botBuys ? curve.mid * (1 + half) : curve.mid * (1 - half)
}

/** Where the share ends after a trade of `sizeUsd` on a wallet worth
 * `valueUsd`: buying cNGN raises it, selling lowers it. */
export function shareAfter(
  share: number,
  sizeUsd: number,
  valueUsd: number,
  botBuys: boolean
): number {
  if (!(valueUsd > 0) || !(sizeUsd > 0)) return clamp01(share)
  const step = sizeUsd / valueUsd
  return clamp01(botBuys ? share + step : share - step)
}

/** The share at which the bot's dollar price reaches the reference buy: past
 * it, the reference market sells dollars cheaper. */
export function cheapestUntil(curve: Curve, halfRangeBps: number): number {
  const target =
    ((1 + halfRangeBps / 10_000) / (1 + curve.allInBps / 10_000) - 1) * 10_000
  const t = (target - curve.s0) / (curve.s1 - curve.s0)
  if (t <= 0) return 0
  if (t >= 1) return 1
  return t ** (1 / curve.shape)
}

/** `a` against `b` in bps, signed. */
export const bpsOver = (a: number, b: number) => (a / b - 1) * 10_000

/** Problems with the draft, in the words the form shows. Empty when valid. */
export function curveConfigErrors(config: Knobs): string[] {
  const errors: string[] = []
  if (!(Number.isFinite(config.start_below_mid) && config.start_below_mid >= 0))
    errors.push('The start offset must be zero or more.')
  if (!(Number.isFinite(config.end_above_buy) && config.end_above_buy >= 0))
    errors.push('The end offset must be zero or more.')
  if (
    !(Number.isFinite(config.shape) && config.shape >= 0.5 && config.shape <= 4)
  )
    errors.push('The shape must be between 0.5 and 4.')
  return errors
}

/** "f²", "f³", "f^2.5": the shape as the operator reads it. */
export function shapeLabel(shape: number): string {
  if (shape === 1) return 'Linear'
  const powers: Record<number, string> = { 2: '²', 3: '³', 4: '⁴' }
  return powers[shape] ? `f${powers[shape]}` : `f^${shape}`
}

/** "Mid", "Mid − 0.5", "Buy + 1": a knob next to the reference it offsets. */
export function knobLabel(reference: 'Mid' | 'Buy', offset: number): string {
  if (!(offset > 0)) return reference
  const amount = offset.toLocaleString(undefined, { maximumFractionDigits: 2 })
  return reference === 'Mid' ? `Mid − ${amount}` : `Buy + ${amount}`
}

/** A check the curve needs before it can price, for the module's editor. */
export interface CurveRequirement {
  label: string
  ok: boolean
  detail: string
}

/** A reading older than this is history, not what the bot sees now. */
export const CURVE_READING_FRESH_SECS = 30

/** The bot's reading, when it is recent enough to show as "now". */
export function freshReading(
  status: CurveStatus | null | undefined,
  now: number
): CurveReading | null {
  const reading = status?.reading ?? null
  return reading && now - reading.at <= CURVE_READING_FRESH_SECS
    ? reading
    : null
}

/** What the curve needs on this bot, judged from what the panel can see.
 * The bot's loader stays authoritative; this says why before it refuses. */
export function curveRequirements(
  view: Pick<
    ModulesView,
    'vault' | 'feed_inverted' | 'corridor_spreads' | 'status' | 'running'
  > & {
    config: Pick<ModulesView['config'], 'inventory'>
  },
  currency: string,
  settlement: string,
  now: number
): CurveRequirement[] {
  const { buy, sell } = view.corridor_spreads
  const sides = [buy, sell].filter((s) => s !== null)
  const bps = sides.every((s) => s.kind === 'bps')
  const equal =
    bps && sides.length > 0 && sides.every((s) => s.value === sides[0]!.value)
  const reading = freshReading(view.status?.curve, now)
  const funding: CurveRequirement = view.vault
    ? {
        label: 'Trades from a vault',
        ok: true,
        detail: `The share is the vault's free ${currency} and ${settlement}, valued as its NAV is.`,
      }
    : {
        label: 'Trades from its own wallet',
        ok: true,
        detail: `The share is this wallet's raw ${currency} and ${settlement} balances.`,
      }
  return [
    funding,
    ...(view.vault
      ? [
          {
            label: 'Inventory balancing off',
            ok: !view.config.inventory.enabled,
            detail: view.config.inventory.enabled
              ? 'Both lean quotes on what the vault holds. Turn inventory balancing off to run the curve.'
              : 'The curve replaces it. Dynamic spreads can stay on.',
          },
        ]
      : []),
    {
      label: 'Equal spreads in bps',
      ok: equal,
      detail: equal
        ? `${sides[0]!.value} bps on both sides. One curve serves both directions.`
        : 'Set the same bps spread on both sides in Corridors. One curve serves both directions.',
    },
    {
      label: 'Feed the right way round',
      ok: !view.feed_inverted,
      detail: view.feed_inverted
        ? `The feed is inverted (a reversed pool). The curve needs ${currency} as the collateral.`
        : `${currency} is the collateral and the feed isn't inverted.`,
    },
    {
      label: 'Reference book on the feed',
      ok: reading !== null,
      detail: reading
        ? `Mid ${reading.reference_mid.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })}, half-range ${reading.half_range_bps.toFixed(1)} bps.`
        : view.running
          ? 'No reference book read yet. The feed has to publish one; Textile’s cNGN feed does.'
          : 'Read once the bot runs. The feed has to publish one; Textile’s cNGN feed does.',
    },
  ]
}

/** The Overview's headline for a bot whose one module is the curve. */
export function curveOverviewState(
  view: Pick<ModulesView, 'running' | 'status' | 'config'>,
  now: number
): { title: string; message: string; pill: string } {
  const mode = view.config.mode
  if (!view.running)
    return {
      title: 'Ready when you are',
      message: 'The bot is stopped. You can still edit its settings.',
      pill: 'Bot stopped',
    }
  if (mode === 'off' || !view.config.curve.enabled)
    return {
      title: 'The curve is off',
      message:
        'Swap quotes use your corridor spread. Its settings stay saved for later.',
      pill: 'Off',
    }
  if (!freshReading(view.status?.curve, now))
    return {
      title: 'Waiting for a reading',
      message:
        'The curve reports once the bot has a fresh price, a reference book and its balances.',
      pill: mode === 'shadow' ? 'Preview mode' : 'Live mode',
    }
  return mode === 'shadow'
    ? {
        title: 'Previewing the curve',
        message:
          'The bot records what the curve would quote. Swap quotes still use your corridor spread until you switch to Live.',
        pill: 'Preview mode',
      }
    : {
        title: 'Quoting off the curve',
        message:
          'Swap quotes and published levels lean on what this wallet holds.',
        pill: 'Live mode',
      }
}

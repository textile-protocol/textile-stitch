// The inventory curve as the panel sees it: the shapes the curve API returns,
// and the curve maths for the Simulate drawer. The maths mirrors
// `src/pricing/curve.rs` and is pinned to the same golden values in
// curve.test.ts, so the example and the bot can't drift apart unnoticed.
//
// Units: everything here is in soft currency per dollar (NGN per USDT), the
// unit the operator, the reference and both knobs live in. The bot's own order
// amounts are the reciprocal; the panel never needs them.

import type { CurveModuleConfig, ModulesConfig, ModulesView } from './modules'
import { isLive, moduleMode } from './moduleModes'
import {
  averageUnwindWeight,
  flooredAskNgn,
  trustedLift,
  unwindWeight,
  type FlatUnwindSide,
  type UnwindLift,
} from './unwind'

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
  /** The feed price the flat quote is built from, on the bot's price basis:
   * `price` in Preview, `unadjustedPrice` in Live where the feed serves one.
   * Not the reference mid. Absent from older bots. */
  feed_price?: number | null
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
  /** The corridor spread plus the venue fee: what a taker pays. */
  allInBps: number
  /** The corridor spread alone, the signed order's half-width. The venue
   * adds its fee on top at fill time, so the bot floors profit unwind's ask
   * on the order price, before the fee. */
  spreadBps: number
  /** Profit unwind's lift on top, when it quotes: `fullNgn` turned into
   * bps against the curve's own reference mid. */
  lift?: UnwindLift
}

/** The curve for this reference and cost, or null when the inputs make none
 * (the bot then quotes flat). `allInBps` is the corridor spread plus the venue
 * fee: the endpoints are the prices a taker actually pays. `spreadBps` is the
 * spread alone (no fee when left out). Negative knobs narrow the curve; once
 * they cross its shades (s1 <= s0, which takes more than crossing the all-in
 * ends) it collapses flat at their midpoint, as the bot does. */
export function curveFor(
  reference: CurveReference,
  allInBps: number,
  config: Knobs,
  spreadBps: number = allInBps
): Curve | null {
  const { mid, half_range_bps: half } = reference
  const { start_below_mid: start, end_above_buy: end, shape } = config
  const finite = [mid, half, allInBps, spreadBps, start, end, shape].every(
    Number.isFinite
  )
  if (!finite || mid <= 0 || half <= 0 || shape <= 0) return null
  if (allInBps < 0 || allInBps >= 10_000) return null
  if (spreadBps < 0 || spreadBps > allInBps) return null
  if (mid - start <= 0) return null
  const buy = mid * (1 + half / 10_000)
  if (buy + end <= 0) return null
  const s0 = ((mid - start) / (mid * (1 + allInBps / 10_000)) - 1) * 10_000
  const s1 = ((buy + end) / (mid * (1 - allInBps / 10_000)) - 1) * 10_000
  if (s1 > s0) return { mid, s0, s1, shape, allInBps, spreadBps }
  const flat = (s0 + s1) / 2
  return { mid, s0: flat, s1: flat, shape, allInBps, spreadBps }
}

const clamp01 = (f: number) => Math.min(1, Math.max(0, f))

/** The same curve with profit unwind's lift on top. A lift of zero, or one
 * it can't trust, leaves the curve plain. Mirrors `Curve::with_lift`. */
export function withLift(curve: Curve, lift: UnwindLift): Curve {
  return { ...curve, lift: trustedLift(lift) ?? undefined }
}

const plainShadeAt = (curve: Curve, f: number) =>
  curve.s0 + (curve.s1 - curve.s0) * clamp01(f) ** curve.shape

function plainAverageShade(curve: Curve, f0: number, f1: number): number {
  const lo = Math.min(clamp01(f0), clamp01(f1))
  const hi = Math.max(clamp01(f0), clamp01(f1))
  if (hi - lo <= Number.EPSILON) return plainShadeAt(curve, lo)
  const p = curve.shape + 1
  return (
    curve.s0 + ((curve.s1 - curve.s0) * (hi ** p - lo ** p)) / (p * (hi - lo))
  )
}

/** The lift at 100% naira in bps off the curve's own reference mid. */
const liftFullBps = (curve: Curve, lift: UnwindLift) =>
  (lift.fullNgn / curve.mid) * 10_000

/** Shade at one naira share, bps off the mid, profit unwind's lift
 * included. */
export function shadeAt(curve: Curve, f: number): number {
  const lift = curve.lift
  return (
    plainShadeAt(curve, f) +
    (lift ? liftFullBps(curve, lift) * unwindWeight(f, lift.startPct) : 0)
  )
}

/** The shade a quote pays: the mean over the stretch of share it moves
 * through, in either direction. A zero-width stretch is the point shade.
 * Profit unwind's lift is averaged the same way. */
export function averageShade(curve: Curve, f0: number, f1: number): number {
  const lift = curve.lift
  return (
    plainAverageShade(curve, f0, f1) +
    (lift
      ? liftFullBps(curve, lift) * averageUnwindWeight(f0, f1, lift.startPct)
      : 0)
  )
}

/** The all-in price a taker gets for the stretch, NGN per USDT: what the bot
 * pays in naira per dollar when it buys cNGN, or takes when it sells. With
 * profit unwind's lift the ask is floored at the cost as the bot does it: on
 * the signed order (spread only), and the venue's fee comes off after. */
export function takerPrice(
  curve: Curve,
  botBuys: boolean,
  f0: number,
  f1: number
): number {
  const centre = (shade: number) => curve.mid * (1 + shade / 10_000)
  const half = curve.allInBps / 10_000
  const lifted = centre(averageShade(curve, f0, f1))
  if (botBuys) return lifted * (1 + half)
  if (!curve.lift) return lifted * (1 - half)
  const spread = curve.spreadBps / 10_000
  const order = flooredAskNgn(
    lifted * (1 - spread),
    centre(plainAverageShade(curve, f0, f1)) * (1 - spread),
    curve.lift.costNgn
  )
  return (order * (1 - half)) / (1 - spread)
}

/** Whether profit unwind's floor stops this ask short of its full lift: the
 * lifted order price would pay out more naira per dollar than the cost. */
export function askFloorBinds(curve: Curve, f0: number, f1: number): boolean {
  const lift = curve.lift
  if (!lift) return false
  const order = (shade: number) =>
    curve.mid * (1 + shade / 10_000) * (1 - curve.spreadBps / 10_000)
  const lifted = order(averageShade(curve, f0, f1))
  return (
    lifted > Math.max(order(plainAverageShade(curve, f0, f1)), lift.costNgn)
  )
}

/** One side of the Simulate example on the curve. Prices are the taker's
 * all-in, NGN per USDT; `lean` is the plain curve against the flat quote and
 * `unwind` profit unwind's lift on top of the plain curve, both in bps, so
 * the two add up to the quote's move off flat when the lift quotes. */
export interface ExampleSide {
  botBuys: boolean
  /** The share the trade leaves the bot at. */
  after: number
  /** What goes out: flat when the reference is too far, the lifted curve
   * when profit unwind quotes, the plain curve otherwise. */
  price: number
  /** The plain curve's price. */
  curved: number
  flat: number | null
  lean: number | null
  /** Null without a lift, or when the bot quotes flat. */
  unwind: number | null
  /** The cost floor stops this sell short of the full lift. */
  atCost: boolean
}

/** One side of an example trade of `sizeUsd` from `share` on a wallet worth
 * `valueUsd`. `lifted` is the plain curve with profit unwind's lift, live or
 * in its preview; only a live one is in `price`. `feed` is the price the bot
 * quotes flat around (null when unknown) and `flatOnly` says the bot quotes
 * flat right now, when `flatUnwind` is profit unwind's lift on that flat
 * quote (`flatUnwindSide`), as the bot applies it then. */
export function exampleSide({
  plain,
  lifted,
  unwindLive,
  feed,
  flatOnly,
  flatUnwind = null,
  share,
  sizeUsd,
  valueUsd,
  botBuys,
}: {
  plain: Curve
  lifted: Curve | null
  unwindLive: boolean
  feed: number | null
  flatOnly: boolean
  flatUnwind?: FlatUnwindSide | null
  share: number
  sizeUsd: number
  valueUsd: number
  botBuys: boolean
}): ExampleSide {
  const after = shareAfter(share, sizeUsd, valueUsd, botBuys)
  const flat = feed === null ? null : flatPrice(feed, plain.allInBps, botBuys)
  const curved = takerPrice(plain, botBuys, share, after)
  if (flatOnly && flat !== null)
    return {
      botBuys,
      after,
      price: flatUnwind?.price ?? flat,
      curved,
      flat,
      lean: null,
      unwind: flatUnwind?.lift ?? null,
      atCost: flatUnwind?.atCost ?? false,
    }
  const unwound = lifted ? takerPrice(lifted, botBuys, share, after) : null
  return {
    botBuys,
    after,
    price: unwound !== null && unwindLive ? unwound : curved,
    curved,
    flat,
    lean: flat === null ? null : bpsOver(curved, flat),
    unwind: unwound === null ? null : bpsOver(unwound, curved),
    atCost: !botBuys && lifted !== null && askFloorBinds(lifted, share, after),
  }
}

/** The feed price the bot quotes flat around, or null when the reading has
 * no usable one. */
export function feedPrice(reading: CurveReading): number | null {
  const { feed_price: price } = reading
  return price != null && Number.isFinite(price) && price > 0 ? price : null
}

/** The flat all-in price around the feed price, for comparison: what the
 * quote would be without the curve. */
export function flatPrice(
  feed: number,
  allInBps: number,
  botBuys: boolean
): number {
  const half = allInBps / 10_000
  return botBuys ? feed * (1 + half) : feed * (1 - half)
}

/** Whether a bot on this config quotes around `unadjustedPrice`, the feed
 * price before manual adjustments: only while the curve runs Live. Otherwise
 * it reads `price`. `Config::price_basis` in src/config.rs. */
export function readsUnadjustedPrice(config: ModulesConfig): boolean {
  return isLive(config, 'curve')
}

/** How far the reference mid or buy may sit from the feed price, bps, before
 * the bot refuses the curve and quotes flat. `MAX_REFERENCE_GAP_BPS` in
 * src/rfq/responder.rs. */
export const MAX_REFERENCE_GAP_BPS = 100

/** Whether the bot quotes flat on this reading because its reference book is
 * too far from the feed price. The bot measures the gap in its own units
 * (settlement per corridor currency), which is `feed / reference` here. */
export function referenceTooFar(reading: CurveReading, feed: number): boolean {
  const mid = reading.reference_mid
  const buy = mid * (1 + reading.half_range_bps / 10_000)
  const gapBps = (reference: number) => Math.abs(feed / reference - 1) * 10_000
  return !(
    gapBps(mid) <= MAX_REFERENCE_GAP_BPS && gapBps(buy) <= MAX_REFERENCE_GAP_BPS
  )
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
  // A collapsed curve is one flat price: cheapest everywhere or nowhere.
  if (curve.s1 - curve.s0 <= Number.EPSILON) return target > curve.s0 ? 1 : 0
  const t = (target - curve.s0) / (curve.s1 - curve.s0)
  if (t <= 0) return 0
  if (t >= 1) return 1
  return t ** (1 / curve.shape)
}

/** `a` against `b` in bps, signed. */
export const bpsOver = (a: number, b: number) => (a / b - 1) * 10_000

/** How far either offset may go, each way, in the soft currency. The editor's
 * controls stop here and Save refuses past it. */
export const MAX_OFFSET = 1000

const offsetInRange = (v: number) =>
  Number.isFinite(v) && v >= -MAX_OFFSET && v <= MAX_OFFSET

/** Problems with the draft, in the words the form shows. Empty when valid. */
export function curveConfigErrors(config: Knobs): string[] {
  const errors: string[] = []
  if (!offsetInRange(config.start_below_mid))
    errors.push(
      `The start offset must be between −${MAX_OFFSET} and ${MAX_OFFSET}.`
    )
  if (!offsetInRange(config.end_above_buy))
    errors.push(
      `The end offset must be between −${MAX_OFFSET} and ${MAX_OFFSET}.`
    )
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

/** "Mid", "Mid − 0.5", "Mid + 0.5", "Buy + 1", "Buy − 0.3": a knob next to
 * the reference it offsets. The start knob counts down from the mid, the end
 * knob up from the buy, so a negative one flips the sign shown. */
export function knobLabel(reference: 'Mid' | 'Buy', offset: number): string {
  if (!Number.isFinite(offset) || offset === 0) return reference
  const amount = Math.abs(offset).toLocaleString(undefined, {
    maximumFractionDigits: 2,
  })
  const up = reference === 'Mid' ? offset < 0 : offset > 0
  return `${reference} ${up ? '+' : '−'} ${amount}`
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
  const mode = moduleMode(view.config, 'curve')
  if (!view.running)
    return {
      title: 'Ready when you are',
      message: 'The bot is stopped. You can still edit its settings.',
      pill: 'Bot stopped',
    }
  if (mode === 'off')
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
          'The bot records what the curve would quote. Swap quotes still use your corridor spread until you set the curve to Live.',
        pill: 'Preview mode',
      }
    : {
        title: 'Quoting off the curve',
        message:
          'Swap quotes and published levels lean on what this wallet holds.',
        pill: 'Live mode',
      }
}

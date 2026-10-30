// Profit unwind as the panel sees it: the shapes the bot reports and the
// maths its editor and examples run. Its own module: it lifts whatever price
// the bot quotes, the inventory curve's or the flat one. The maths mirrors
// `src/pricing/unwind.rs` and is pinned to it by unwindParity.json.
//
// Units: everything here is in soft currency per dollar (NGN per USDT), like
// the curve.

import type { ModulesView, ProfitUnwindConfig } from './modules'
import { moduleMode } from './moduleModes'

/** What profit unwind sees this tick. Mirrors `UnwindReading` in
 * src/modules/runtime.rs. */
export interface UnwindReading {
  at: number
  /** The corridor currency's share of what the bot holds, 0..1, valued at
   * the feed; null without a fresh balance reading. */
  share: number | null
  /** What the bot holds, valued in the settlement currency. */
  value: number | null
  /** The venue fee on the last quote request, bps. */
  fee_bps: number
  /** The frozen average cost the lift is sized on, and when it froze. */
  cost: number
  snapshot_at: number
  /** The feed price the gain is measured against. */
  feed_mid: number
  /** Cost minus the feed: positive is a gain per dollar. */
  gain: number
  /** How far both sides lift at 100% naira; at share f, this times the
   * weight at f. */
  full_shift: number
  /** Whether the lift quotes, or is profit unwind's preview. */
  live: boolean
}

/** One Swap request profit unwind lifted, or would have in its preview. */
export interface UnwindQuote {
  at: number
  bot_buys: boolean
  live: boolean
  /** Settlement units. */
  size: number
  share_before: number
  share_after: number
  /** The lifted price against the one it lifted (the curve's or the flat
   * one), bps of NGN per USDT: positive gives more naira per dollar. */
  lift_bps: number
}

export interface UnwindStatus {
  reading: UnwindReading | null
  /** Oldest first, up to 200. */
  quotes: UnwindQuote[]
}

/** Profit unwind's lift as pricing applies it: an extra `fullNgn *
 * weight(f)` on whichever price goes out, averaged over the stretch a quote
 * walks, with every ask it moves floored at the cost. Mirrors `Lift`. */
export interface UnwindLift {
  /** The lift at 100% naira, NGN per USDT. */
  fullNgn: number
  startPct: number
  /** The frozen average cost, NGN per USDT. */
  costNgn: number
}

/** A reading older than this is history, not what the bot sees now. */
export const UNWIND_READING_FRESH_SECS = 30

/** The bot's reading, when it is recent enough to show as "now". */
export function freshUnwindReading(
  status: UnwindStatus | null | undefined,
  now: number
): UnwindReading | null {
  const reading = status?.reading ?? null
  return reading && now - reading.at <= UNWIND_READING_FRESH_SECS
    ? reading
    : null
}

const clamp01 = (f: number) => Math.min(1, Math.max(0, f))

const startFraction = (startPct: number) =>
  Math.min(0.99, Math.max(0, startPct / 100))

/** How much of the full lift applies at naira share `f`: none at or below
 * the start, then rising evenly to all of it at 100%. Mirrors `weight`. */
export function unwindWeight(f: number, startPct: number): number {
  const start = startFraction(startPct)
  const share = clamp01(f)
  return share <= start ? 0 : (share - start) / (1 - start)
}

/** The mean of `unwindWeight` over the stretch between `f0` and `f1`, in
 * either order: what a quote walking it pays. Mirrors `average_weight`. */
export function averageUnwindWeight(
  f0: number,
  f1: number,
  startPct: number
): number {
  const lo = Math.min(clamp01(f0), clamp01(f1))
  const hi = Math.max(clamp01(f0), clamp01(f1))
  if (hi - lo <= Number.EPSILON) return unwindWeight(lo, startPct)
  const start = startFraction(startPct)
  const area = (f: number) =>
    f <= start ? 0 : (f - start) ** 2 / (2 * (1 - start))
  return (area(hi) - area(lo)) / (hi - lo)
}

/** The full lift (at 100% naira) in NGN per USDT for the draft's knobs,
 * from the bot's frozen cost and the feed it last read. Mirrors `shift`, so
 * the editor shows what saving would quote. */
export function unwindFullNgn(
  knobs: Pick<ProfitUnwindConfig, 'share' | 'cap_ngn' | 'min_gain_ngn'>,
  reading: Pick<UnwindReading, 'cost' | 'feed_mid'>
): number {
  const { cost, feed_mid: mid } = reading
  if (!(Number.isFinite(mid) && mid > 0 && Number.isFinite(cost))) return 0
  const gain = cost - mid
  if (gain <= 0 || gain < knobs.min_gain_ngn) return 0
  const full = Math.min(Math.max(0, knobs.cap_ngn), clamp01(knobs.share) * gain)
  return Number.isFinite(full) ? full : 0
}

/** The draft's lift off the bot's frozen cost, live or preview; null while
 * it is off or the bot hasn't reported a cost. */
export function draftUnwindLift(
  config: ProfitUnwindConfig,
  reading: UnwindReading | null
): UnwindLift | null {
  if (!config.enabled || !reading) return null
  return {
    fullNgn: unwindFullNgn(config, reading),
    startPct: config.start_pct,
    costNgn: reading.cost,
  }
}

/** The ask with the lift, NGN per USDT: raised towards the cost but never
 * past it, and never below where the plain price already is. Mirrors
 * `floored_ask_ngn`. */
export function flooredAskNgn(
  lifted: number,
  plain: number,
  costNgn: number
): number {
  if (!(Number.isFinite(costNgn) && costNgn > 0)) return plain
  return Math.min(lifted, Math.max(plain, costNgn))
}

/** The biggest full lift, NGN per USDT, at which a lift that falls between
 * a taker's legs can't pay them: draining the whole book collects at most
 * `full * (1 - start) / 2`, and a round trip is only sure to cost twice the
 * spread (the venue fee is per request). Advisory: the panel warns above it
 * and the bot quotes the cap as set. Mirrors `lift_limit_ngn`. */
export function liftLimitNgn(
  spreadBps: number,
  midNgn: number,
  startPct: number
): number {
  return (4 * (spreadBps / 10_000) * midNgn) / (1 - startFraction(startPct))
}

/** The cap field's hint. Save doesn't stop a cap above the round-trip limit,
 * so the hint always names the risk: against `limitNgn` when there's one,
 * and in words when the panel can't size it (no fresh reading, or no bps
 * spread). `price` formats NGN. */
export function unwindCapHint(
  capNgn: number,
  limitNgn: number | null,
  settlement: string,
  price: (n: number) => string
): string {
  const most = `The most both prices ever move, per ${settlement}.`
  const risk =
    'a taker can profit from the lift falling between trades, out of the gain you give back. Every sale still clears at or above the cost.'
  if (limitNgn === null)
    return `${most} The panel can't size the round-trip limit without a fresh reading and a bps spread. Above it, ${risk}`
  return capNgn > limitNgn
    ? `${most} Above ${price(limitNgn)} at today's spread and start, ${risk}`
    : `${most} Up to ${price(limitNgn)} at today's spread and start, a taker can't profit from the lift falling between trades.`
}

/** The lift, or null when it lifts nothing: a lift of zero, or one it can't
 * trust. Mirrors `Lift::trusted`. */
export function trustedLift(lift: UnwindLift): UnwindLift | null {
  const trusted =
    Number.isFinite(lift.fullNgn) &&
    Number.isFinite(lift.startPct) &&
    Number.isFinite(lift.costNgn) &&
    lift.costNgn > 0
  return trusted && lift.fullNgn > 0 ? lift : null
}

/** A flat signed order with the lift for the stretch `f0..f1`, NGN per
 * USDT: the full lift in bps of `midNgn`, the flat centre, averaged over the
 * stretch, raises both sides, and the ask stops at the cost. `lift` is
 * already checked. Mirrors `lifted_flat_price`, in NGN. */
export function liftedFlatOrderNgn(
  orderNgn: number,
  botBuys: boolean,
  lift: UnwindLift,
  midNgn: number,
  f0: number,
  f1: number
): number {
  const liftBps =
    (lift.fullNgn / midNgn) *
    10_000 *
    averageUnwindWeight(f0, f1, lift.startPct)
  if (
    !(
      Number.isFinite(orderNgn) &&
      orderNgn > 0 &&
      Number.isFinite(liftBps) &&
      liftBps > 0
    )
  )
    return orderNgn
  const lifted = orderNgn * (1 + liftBps / 10_000)
  return botBuys ? lifted : flooredAskNgn(lifted, orderNgn, lift.costNgn)
}

/** A flat signed order around `feed`, NGN per USDT: a bps spread off the
 * feed in the bot's units (debt per collateral), read back in NGN. Mirrors
 * `bid_price` and `ask_price` for a bps spread. */
export function flatOrderNgn(
  feed: number,
  spreadBps: number,
  botBuys: boolean
): number {
  const half = spreadBps / 10_000
  return botBuys ? feed / (1 - half) : feed / (1 + half)
}

/** One side of profit unwind's Simulate example on a flat price. */
export interface FlatUnwindSide {
  /** With the lift, live or in its preview; null without one. */
  unwound: number | null
  /** What goes out: lifted only while the lift quotes. */
  price: number
  /** The lift against the flat quote, bps; null without one. */
  lift: number | null
  /** The cost floor stops this sell short of the full lift. */
  atCost: boolean
}

/** Profit unwind's lift on a flat quote for the stretch `share..after`.
 * `base` is the flat all-in quote the caller shows; the lift moves it as it
 * moves the flat signed order `order` (`flatOrderNgn`), so the venue fee on
 * top is left as it is. `lift` is already checked (`trustedLift`). */
export function flatUnwindSide({
  base,
  order,
  lift,
  live,
  midNgn,
  share,
  after,
  botBuys,
}: {
  base: number
  order: number
  lift: UnwindLift | null
  live: boolean
  midNgn: number
  share: number
  after: number
  botBuys: boolean
}): FlatUnwindSide {
  if (!lift) return { unwound: null, price: base, lift: null, atCost: false }
  const lifted = liftedFlatOrderNgn(order, botBuys, lift, midNgn, share, after)
  const full =
    order *
    (1 +
      (lift.fullNgn / midNgn) *
        averageUnwindWeight(share, after, lift.startPct))
  const unwound = base * (lifted / order)
  return {
    unwound,
    price: live ? unwound : base,
    lift: (unwound / base - 1) * 10_000,
    atCost: !botBuys && lifted < full - 1e-9,
  }
}

/** The corridor's smaller bps spread, the one the bot holds a flat lift's
 * limit to, or null when either side is absolute or none quotes. */
export function minSpreadBps(
  view: Pick<ModulesView, 'corridor_spreads'>
): number | null {
  const sides = [view.corridor_spreads.buy, view.corridor_spreads.sell].filter(
    (s) => s !== null
  )
  if (!sides.length || !sides.every((s) => s.kind === 'bps')) return null
  return Math.min(...sides.map((s) => s.value))
}

/** Problems with the draft, in the words the form shows. */
export function profitUnwindErrors(config: ProfitUnwindConfig): string[] {
  const errors: string[] = []
  if (
    !(Number.isFinite(config.share) && config.share >= 0 && config.share <= 1)
  )
    errors.push('The share of the gain must be 0–100%.')
  if (!(Number.isFinite(config.cap_ngn) && config.cap_ngn >= 0))
    errors.push('The cap must be zero or more.')
  if (!(Number.isFinite(config.min_gain_ngn) && config.min_gain_ngn >= 0))
    errors.push('The smallest gain must be zero or more.')
  if (
    !(
      Number.isFinite(config.start_pct) &&
      config.start_pct >= 0 &&
      config.start_pct <= 99
    )
  )
    errors.push('The start must be 0–99%.')
  if (!(Number.isSafeInteger(config.refresh_secs) && config.refresh_secs >= 60))
    errors.push('The refresh must be at least 60 seconds.')
  return errors
}

/** A check profit unwind needs before it can lift, for the editor. */
export interface UnwindRequirement {
  label: string
  ok: boolean
  detail: string
}

/** What profit unwind needs on this bot, judged from what the panel can
 * see. The bot's loader stays authoritative; this says why before it
 * refuses. */
export function unwindRequirements(
  view: Pick<
    ModulesView,
    'vault' | 'feed_inverted' | 'corridor_spreads' | 'status' | 'running'
  > & {
    config: Pick<ModulesView['config'], 'inventory'>
  },
  currency: string,
  settlement: string,
  now: number
): UnwindRequirement[] {
  const spread = minSpreadBps(view)
  const reading = freshUnwindReading(view.status?.profit_unwind, now)
  return [
    ...(view.vault
      ? [
          {
            label: 'Inventory balancing off',
            ok: !view.config.inventory.enabled,
            detail: view.config.inventory.enabled
              ? 'It narrows a side below your spread, which the round-trip limit counts on. Turn inventory balancing off to run profit unwind.'
              : 'Dynamic spreads can stay on: they only widen the spread.',
          },
        ]
      : []),
    {
      label: 'Spreads in bps',
      ok: spread !== null,
      detail:
        spread !== null
          ? `The lift moves both sides by the same bps, and the cap's round-trip limit is a multiple of the ${spread} bps configured spread.`
          : 'Set bps spreads in Corridors. The lift moves both sides by the same bps, and the round-trip limit is a multiple of the spread.',
    },
    {
      label: 'Feed the right way round',
      ok: !view.feed_inverted,
      detail: view.feed_inverted
        ? `The feed is inverted (a reversed pool). Profit unwind tracks what ${currency} cost, so it needs ${currency} as the collateral.`
        : `${currency} is the collateral and the feed isn't inverted.`,
    },
    {
      label: 'A cost to measure against',
      ok: reading !== null,
      detail: reading
        ? `The ${view.vault ? 'vault' : 'wallet'}'s ${currency} cost ${reading.cost.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })} per ${settlement} on average.`
        : view.running
          ? `Tracked once the running bot reads its balances with profit unwind on. It starts at the feed, so nothing lifts until a gain builds.`
          : 'Tracked once the bot runs with profit unwind on.',
    },
  ]
}

/** The Overview's headline for profit unwind. */
export function unwindOverviewState(
  view: Pick<ModulesView, 'running' | 'status' | 'config'>,
  now: number
): { title: string; message: string; pill: string } {
  const mode = moduleMode(view.config, 'profit_unwind')
  const live = mode === 'live'
  if (!view.running)
    return {
      title: 'Ready when you are',
      message: 'The bot is stopped. You can still edit its settings.',
      pill: 'Bot stopped',
    }
  if (mode === 'off')
    return {
      title: 'Profit unwind is off',
      message:
        'Quotes carry no lift. Its settings stay saved for later, and the cost starts over at the feed once it is back on.',
      pill: 'Off',
    }
  const reading = freshUnwindReading(view.status?.profit_unwind, now)
  if (!reading)
    return {
      title: 'Waiting for a reading',
      message:
        'Profit unwind reports once the bot has a fresh price and its balances.',
      pill: live ? 'Live' : 'Preview',
    }
  if (reading.full_shift <= 0)
    return {
      title: 'Nothing to give back',
      message:
        reading.gain > 0
          ? 'The gain on what the bot holds is under your minimum, so quotes carry no lift.'
          : 'What the bot holds is worth no more than it cost, so quotes carry no lift.',
      pill: live ? 'Live' : 'Preview',
    }
  return live
    ? {
        title: 'Giving part of the gain back',
        message:
          'Swap quotes and published levels carry the lift, so takers bringing dollars clear the pile faster. Sells never go past the average cost.',
        pill: 'Live',
      }
    : {
        title: 'Previewing the lift',
        message:
          'The bot records what profit unwind would quote. Swap quotes carry no lift until you set it to Live.',
        pill: 'Preview',
      }
}

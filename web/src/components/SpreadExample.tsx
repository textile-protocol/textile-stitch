// The Spread step's worked example: what two bps inputs do to the price the
// bot will quote around. With the feed's mid in hand (see useFeedMid) it
// shows the real bid and ask; without one it falls back to a swap worth 1,000
// of the quote token, which is right whatever the mid is. Pure props, no
// fetch: the caller decides whether a mid exists.

import type { ReactNode } from 'react'
import { formatClock } from '../format'
import type { FeedMid } from '../api'
import type { Spread } from '../types'
import type { FeedMidState } from './wizard/useFeedMid'

/**
 * The example swap is worth this much of the quote token at the mid. Price-free
 * by construction: 1 bps of it is 0.10 whatever the corridor trades at.
 */
export const EXAMPLE_NOTIONAL = 1000

/** Named quick picks. `bps` is per side: the chip sets both sides to it. */
export const SPREAD_BANDS = [
  { name: 'Tight', bps: 1 },
  { name: 'Normal', bps: 3 },
  { name: 'Wide', bps: 10 },
] as const

/**
 * The backend parses a bps spread as a u32, so this is the largest string it
 * will take. Anything above it fails at Create with a parse error.
 */
const MAX_BPS = 4_294_967_295

/**
 * At 10,000 bps the bid prices at zero: the backend refuses a buy spread at or
 * above it, so the example's buy side stops there and says why. A sell spread
 * has no such bound (the ask only climbs), so the sell side draws any whole
 * number the backend would accept.
 */
export const BPS_LIMIT = 10_000

type Side = 'buy' | 'sell'

/**
 * A spread input as a whole number of bps, or null when it isn't one (empty,
 * a decimal, negative, not a number, above what a u32 holds). Mirrors the
 * backend, which parses the field as an unsigned integer and rejects
 * "0.25", "5.", "1e3" and friends outright. Shared with the wizard's Next
 * gate so the example and the gate can never disagree about what counts as
 * a spread.
 */
export function parseBps(v: string): number | null {
  const t = v.trim()
  if (!/^\d+$/.test(t)) return null
  const n = Number(t)
  return n <= MAX_BPS ? n : null
}

/**
 * "cNGN / USDT" → base cNGN, quote USDT. The catalog joins with " / ", the
 * registry with "→" or "↔"; the rest are free insurance. Null for anything that
 * isn't exactly two symbols, so an unknown format degrades to unitless amounts
 * rather than a wrong unit.
 */
export function pairSymbols(
  displayName: string | undefined,
): { base: string; quote: string } | null {
  if (!displayName) return null
  const [base, quote, ...rest] = displayName
    .split(/\s*(?:\/|→|↔|⇄|=>|->)\s*/)
    .map((p) => p.trim())
  if (!base || !quote || rest.length > 0) return null
  return { base, quote }
}

// Whole bps on a 1,000 notional are exact at two decimals (1 bps = 0.10).
const money = new Intl.NumberFormat(undefined, {
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
})
const whole = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 })

/**
 * How a live mid is shown. The feed publishes quote per base (USDT per cNGN,
 * 0.00073), which is the bot's fair price but not how anyone says it: the
 * market, and the Textile app, say 1,368 cNGN per USDT. So a pair whose base
 * is worth less than one unit of quote is turned round for display, and the
 * bid and ask are turned round with it. Nothing about the bot changes; the
 * bps are applied to the feed's number and only the last step inverts.
 */
export function displayRate(mid: number): { inverted: boolean; rate: number } {
  return mid < 1 ? { inverted: true, rate: 1 / mid } : { inverted: false, rate: mid }
}

/** Enough decimals to see one bps move, without a wall of digits. */
function rateFormatter(rate: number): Intl.NumberFormat {
  const digits = rate >= 1000 ? 2 : rate >= 10 ? 3 : rate >= 1 ? 4 : 6
  return new Intl.NumberFormat(undefined, {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  })
}

/**
 * A side's bps for display: a whole number the backend would accept. The buy
 * side stops where the backend does; the sell side has no ceiling.
 */
function shownBps(v: string, side: Side): number | null {
  const n = parseBps(v)
  if (n === null) return null
  return side === 'buy' && n >= BPS_LIMIT ? null : n
}

/**
 * What to show in place of a side's amount when there is none. Named so the
 * operator fixes the input here, not at the Wallet step where the backend
 * would otherwise be the first to complain. The last case is the one that
 * also holds Next: a buy spread the backend refuses, so it says the limit
 * rather than sounding like a display quirk.
 */
function placeholder(v: string, side: Side): string {
  const t = v.trim()
  if (t.startsWith('-')) return '0 or more'
  if (parseBps(t) === null) return 'enter a whole number'
  return side === 'buy'
    ? `must be under ${whole.format(BPS_LIMIT)} bps`
    : 'too wide to show'
}

/** Where the end of the scale sits, in percent of the line's width from the
 * centre. The line IS the scale, so its ends are the scale's ends: a side at
 * the top of the range lands on the end, next to the number that names it. */
const DOT_MAX_PCT = 50

/**
 * The scale the line is drawn on: 0 to this many bps on each side. The first
 * rung that holds the wider side, so a spread is read against a fixed range
 * rather than against the other side. Past the last rung the scale stops
 * growing, which only matters above the 10,000 bps the backend refuses on the
 * buy side anyway.
 */
const SCALE_LADDER = [10, 25, 50, 100, 250, 500, 1000, 2500, 5000] as const
/** The widest scale drawn. Above it the dots pin to the ends. */
const SCALE_TOP = 10000

function axisMax(maxBps: number): number {
  return SCALE_LADDER.find((rung) => maxBps <= rung) ?? SCALE_TOP
}

/**
 * Where a side's dot sits, in percent of the line's width from the centre: its
 * share of the fixed scale. On a 0 to 50 range 30 bps sits well out and 1 bps
 * sits near the mid, because that is what 30 against 1 looks like. Two earlier
 * versions were relative (a 20% floor with a 25 bps cap, then scaling the two
 * sides against each other), and both drew unequal spreads as near-symmetric.
 */
function dotOffsetPct(bps: number, axis: number): number {
  if (bps <= 0 || axis <= 0) return 0
  return (DOT_MAX_PCT * Math.min(bps, axis)) / axis
}

/**
 * A caption's position, held far enough inside the line to stay under it. The
 * dot itself is never clamped; only its word is, and only at the very ends,
 * where the caption is wider than the room left beside it.
 */
function captionPct(pct: number): number {
  return Math.min(92, Math.max(8, pct))
}

/**
 * The worked example under the two spread inputs. Everything is in the quote
 * token on a swap worth 1,000 at the mid, so it never needs a price and never
 * has to say which way the feed quotes the pair: "pays less" and "charges
 * more" read the same whether the operator thinks in USDT per cNGN or cNGN
 * per USDT. Renders nothing for absolute offsets, which can't be read as a
 * share of the notional without a price. If a feed mid is ever proxied
 * through the panel it belongs on one faint line under the header; nothing
 * else here would change.
 */
export function SpreadExample({
  buy,
  sell,
  base,
  quote,
  feed = { status: 'idle' },
}: {
  buy: Spread
  sell: Spread
  /** Symbols from the corridor name; null for a custom or imported corridor. */
  base: string | null
  quote: string | null
  /** The feed's live mid, when the caller fetched one. Idle means "don't". */
  feed?: FeedMidState
}) {
  if (buy.kind !== 'bps' || sell.kind !== 'bps') return null

  const b = shownBps(buy.value, 'buy')
  const s = shownBps(sell.value, 'sell')
  const unit = quote ? ` ${quote}` : ''
  const amount = (n: number) => money.format(n) + unit

  const buyEdge = b === null ? null : (EXAMPLE_NOTIONAL * b) / 10000
  const sellEdge = s === null ? null : (EXAMPLE_NOTIONAL * s) / 10000

  const live = feed.status === 'ok' ? liveExample(feed.mid, b, s, base, quote) : null

  // One fixed scale for both sides, stepped up to hold the wider one, so the
  // dots show each spread's size and not just their ratio.
  const axis = axisMax(Math.max(b ?? 0, s ?? 0))
  const buyPct = b === null ? 0 : dotOffsetPct(b, axis)
  const sellPct = s === null ? 0 : dotOffsetPct(s, axis)

  // On a phone each cell is a row: label left, amount right, sub-line under
  // the amount. From `sm` up the three sit side by side, centred. The blank
  // sub-line only exists in the column layout, to keep the cells one height.
  const cell = (label: string, value: ReactNode, sub: string) => (
    <div className="flex min-w-0 flex-wrap items-baseline justify-between gap-x-3 sm:block sm:text-center">
      <span className="text-xs text-muted sm:block">{label}</span>
      <span className="text-base font-bold tabular-nums sm:block">{value}</span>
      <span
        className={
          sub
            ? 'basis-full text-right text-xs text-faint sm:text-center'
            : 'hidden text-xs sm:block'
        }
      >
        {sub || ' '}
      </span>
    </div>
  )
  const missing = (v: string, side: Side) => (
    <span className="text-sm font-normal text-faint">
      {placeholder(v, side)}
    </span>
  )

  // The two outer cells, each still driven by its own spread input.
  const buyCell = cell(
    live ? live.buyLabel : base ? `You buy ${base} for` : 'You buy for',
    b === null || buyEdge === null
      ? missing(buy.value, 'buy')
      : live
        ? live.buy
        : amount(EXAMPLE_NOTIONAL - buyEdge),
    buyEdge === null ? '' : live ? live.buySub : `${amount(buyEdge)} less than the mid`,
  )
  const midCell = cell('Mid', live ? live.mid : amount(EXAMPLE_NOTIONAL), live ? live.midSub : '')
  const sellCell = cell(
    live ? live.sellLabel : base ? `You sell ${base} for` : 'You sell for',
    s === null || sellEdge === null
      ? missing(sell.value, 'sell')
      : live
        ? live.sell
        : amount(EXAMPLE_NOTIONAL + sellEdge),
    sellEdge === null ? '' : live ? live.sellSub : `${amount(sellEdge)} more than the mid`,
  )

  // Low number on the left, high on the right, like any quote. On a pair the
  // example shows the other way round (cNGN per USDT) the buy-spread side is
  // the HIGHER number, so the whole picture mirrors: the sell-spread cell and
  // its dot take the left, the buy-spread cell and dot take the right. Each
  // dot still tracks its own input; only where it is drawn changes.
  const mirror = live?.inverted ?? false
  const [leftCell, rightCell] = mirror ? [sellCell, buyCell] : [buyCell, sellCell]
  const leftBps = mirror ? s : b
  const rightBps = mirror ? b : s
  const leftPct = mirror ? sellPct : buyPct
  const rightPct = mirror ? buyPct : sellPct
  const leftCaption = mirror ? `buys ${live?.quoteName} lower` : 'pays less'
  const rightCaption = mirror ? `sells ${live?.quoteName} higher` : 'charges more'
  // A caption needs room between its dot and the mid ring, or the three words
  // print on top of each other at a tight spread. Under this it stays quiet;
  // the numbers above already say what the dot means.
  const captionRoom = 9

  return (
    <div className="mt-4 rounded-lg border border-line-soft bg-canvas p-4">
      {live ? (
        <p className="text-xs font-bold text-muted">
          Your price feed's mid as of {formatClock(live.readAt * 1000)}. 1 bps = 0.01% of it.
        </p>
      ) : (
        <p className="text-xs font-bold text-muted">
          {feed.status === 'loading'
            ? 'Reading your price feed…'
            : feed.status === 'down'
              ? 'Your price feed did not answer, so an example instead:'
              : 'Example:'}{' '}
          a swap worth {whole.format(EXAMPLE_NOTIONAL)}
          {unit} at the mid, so 1 bps = 0.01%, or {amount(EXAMPLE_NOTIONAL / 10000)}
        </p>
      )}

      <div className="mt-3 grid grid-cols-1 gap-x-2 gap-y-2 sm:grid-cols-3">
        {leftCell}
        {midCell}
        {rightCell}
      </div>

      {/* Dot positions are inline styles on purpose: Tailwind v4 can't see a
          class built from a template string, so `left-[${x}%]` would never
          paint. The mid ring is last so it sits on top of a dot at 0 bps. */}
      <div className="mt-3 flex items-start gap-2">
        {/* The scale's ends live outside the line, so the line's full width is
            the range itself. A label sitting at the container edge while the
            dots stopped short of it read as a contradiction: 100 on a 100 bps
            scale drew four fifths of the way out. */}
        <span aria-hidden className="mt-[3px] shrink-0 text-[10px] text-faint">
          {whole.format(axis)}
        </span>
        <div className="min-w-0 flex-1">
      <div aria-hidden className="relative h-4">
        <div className="absolute inset-x-0 top-1/2 h-px bg-line" />
        {leftBps !== null && rightBps !== null && (
          <div
            className="absolute top-1/2 h-1.5 -translate-y-1/2 rounded-full bg-accent/25 transition-[left,width] duration-200"
            style={{
              left: `${50 - leftPct}%`,
              width: `${leftPct + rightPct}%`,
            }}
          />
        )}
        {leftBps !== null && (
          <span
            className="absolute top-1/2 size-2.5 -translate-x-1/2 -translate-y-1/2 rounded-full bg-accent transition-[left] duration-200"
            style={{ left: `${50 - leftPct}%` }}
          />
        )}
        {rightBps !== null && (
          <span
            className="absolute top-1/2 size-2.5 -translate-x-1/2 -translate-y-1/2 rounded-full bg-accent transition-[left] duration-200"
            style={{ left: `${50 + rightPct}%` }}
          />
        )}
        <span className="absolute left-1/2 top-1/2 size-3 -translate-x-1/2 -translate-y-1/2 rounded-full border-2 border-ink bg-canvas" />
      </div>
      {/* Captions ride with the dots and say what the dot does in money, not
          which way the feed quotes. A side at 0 bps sits inside the ring, so
          its caption gives way to "mid" rather than overprinting it. */}
      <div aria-hidden className="relative h-4 text-[10px] text-faint">
        {leftBps !== null && leftPct >= captionRoom && (
          <span
            className="absolute -translate-x-1/2 whitespace-nowrap transition-[left] duration-200"
            style={{ left: `${captionPct(50 - leftPct)}%` }}
          >
            {leftCaption}
          </span>
        )}
        <span className="absolute left-1/2 -translate-x-1/2">mid</span>
        {rightBps !== null && rightPct >= captionRoom && (
          <span
            className="absolute -translate-x-1/2 whitespace-nowrap transition-[left] duration-200"
            style={{ left: `${captionPct(50 + rightPct)}%` }}
          >
            {rightCaption}
          </span>
        )}
      </div>
        </div>
        <span aria-hidden className="mt-[3px] shrink-0 text-[10px] text-faint">
          {whole.format(axis)}
        </span>
      </div>
    </div>
  )
}

/**
 * The three cells on a live mid. Bid and ask come off the feed's own number
 * (quote per base): the bot pays mid × (1 − buy) and charges mid × (1 + sell).
 *
 * Everything is said in the hard currency. For a pair that reads as soft per
 * hard (cNGN per USDT), the bot buying cNGN IS the bot selling USDT, so the
 * buy-spread cell becomes "You sell USDT for 1,368.77 cNGN" and the
 * sell-spread cell "You buy USDT for 1,368.49 cNGN": more cNGN when it sells a
 * dollar, fewer when it buys one, which is how the operator already thinks
 * about it. A pair that reads as hard per unit (3,400 USDT per XAUt) is
 * already in the hard currency and keeps "You buy XAUt for 3,399.66 USDT".
 */
function liveExample(
  feedMid: FeedMid,
  b: number | null,
  s: number | null,
  base: string | null,
  quote: string | null,
): {
  inverted: boolean
  quoteName: string
  mid: string
  midSub: string
  buyLabel: string
  buy: string
  buySub: string
  sellLabel: string
  sell: string
  sellSub: string
  /** When the feed published this mid, unix seconds. */
  readAt: number
} {
  const { inverted, rate } = displayRate(feedMid.price)
  const fmt = rateFormatter(rate)
  const baseName = base ?? 'base'
  const quoteName = quote ?? 'quote'
  // The unit the numbers are in: cNGN when inverted, USDT when not.
  const unit = inverted ? baseName : quoteName
  const show = (n: number) => `${fmt.format(n)} ${unit}`

  const bidQpb = b === null ? null : feedMid.price * (1 - b / 10000)
  const askQpb = s === null ? null : feedMid.price * (1 + s / 10000)
  const toShown = (qpb: number) => (inverted ? 1 / qpb : qpb)

  const bid = bidQpb === null ? null : toShown(bidQpb)
  const ask = askQpb === null ? null : toShown(askQpb)
  const delta = (x: number) => `${fmt.format(Math.abs(x - rate))} ${unit}`

  return {
    inverted,
    quoteName,
    readAt: feedMid.timestamp,
    mid: show(rate),
    midSub: inverted
      ? `per ${quoteName}. 1 ${baseName} = ${feedMid.price.toPrecision(4)} ${quoteName}`
      : `per ${baseName}`,
    buyLabel: inverted ? `You sell ${quoteName} for` : `You buy ${baseName} for`,
    buy: bid === null ? '' : show(bid),
    buySub: bid === null ? '' : `${delta(bid)} ${inverted ? 'more' : 'less'} than the mid`,
    sellLabel: inverted ? `You buy ${quoteName} for` : `You sell ${baseName} for`,
    sell: ask === null ? '' : show(ask),
    sellSub: ask === null ? '' : `${delta(ask)} ${inverted ? 'fewer' : 'more'} than the mid`,
  }
}

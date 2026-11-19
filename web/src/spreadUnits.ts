// A side's spread in either unit, and what it quotes at a mid.
//
// The bot prices an absolute offset in collateral per debt: on a forward
// cNGN/USDT pool that's cNGN per USDT, the "local currency per dollar" rate
// operators quote. The feed publishes the other way up (USDT per cNGN), so
// everything here works on `1 / price`. The math mirrors `pricing::quote` in
// the bot: a bid moves collateral per debt up by the offset (buy cheaper), an
// ask moves it down (sell dearer), and `min_spread_bps` floors an absolute
// side at that many bps.

import type { Spread } from './types'

export type Side = 'buy' | 'sell'

/** The two token symbols in a pool label like `cNGN / USDT` or `cNGN → USDT`,
 * or null for a custom pool labelled by addresses. */
export function pairSymbols(label: string): { collateral: string; debt: string } | null {
  const [collateral, debt, ...rest] = label.split(/\s*(?:\/|→)\s*/).map((s) => s.trim())
  const symbol = (p: string | undefined): p is string =>
    !!p && !p.includes('…') && !p.startsWith('0x')
  if (rest.length > 0 || !symbol(collateral) || !symbol(debt)) return null
  return { collateral, debt }
}

/** The absolute unit for a pool, e.g. `cNGN per USDT`. */
export function absoluteUnit(label: string): string {
  const symbols = pairSymbols(label)
  return symbols ? `${symbols.collateral} per ${symbols.debt}` : 'collateral per debt'
}

/** The bps price (debt per collateral) of a side: what `bid_price` and
 * `ask_price` return for `Spread::Bps`. */
const bpsPrice = (side: Side, mid: number, bps: number) =>
  side === 'buy' ? mid * (1 - Math.min(bps, 10_000) / 10_000) : mid * (1 + bps / 10_000)

/** The absolute price of a side, 0 when the offset runs past the mid. */
const absPrice = (side: Side, mid: number, abs: number) => {
  const perDebt = side === 'buy' ? 1 / mid + abs : 1 / mid - abs
  return perDebt > 0 ? 1 / perDebt : 0
}

export interface SideQuote {
  /** Collateral per debt the side quotes at, e.g. 1531 cNGN per USDT. */
  rate: number
  /** How far that is from the mid, in bps. */
  bps: number
  /** The floor set the price, not the absolute offset. */
  floored: boolean
}

/**
 * What one side quotes at `mid` (debt per collateral, as the feed publishes
 * it). Null when the spread isn't a usable number, the mid isn't positive, or
 * the side prices to nothing.
 */
export function sideQuote(
  side: Side,
  spread: Spread,
  mid: number,
  minSpreadBps: string,
): SideQuote | null {
  const value = Number(spread.value)
  if (spread.value.trim() === '' || !Number.isFinite(value) || value < 0) return null
  if (!Number.isFinite(mid) || mid <= 0) return null
  const floor = Number(minSpreadBps)
  const hasFloor = spread.kind === 'abs' && minSpreadBps.trim() !== '' && floor > 0
  let price = spread.kind === 'bps' ? bpsPrice(side, mid, value) : absPrice(side, mid, value)
  let floored = false
  if (hasFloor && price > 0) {
    const floorPrice = bpsPrice(side, mid, floor)
    const wider = side === 'buy' ? floorPrice < price : floorPrice > price
    if (wider) {
      price = floorPrice
      floored = true
    }
  }
  if (!(price > 0)) return null
  const bps = side === 'buy' ? (1 - price / mid) * 10_000 : (price / mid - 1) * 10_000
  return { rate: 1 / price, bps, floored }
}

/**
 * The same spread in the other unit, priced at `mid`, so a unit switch doesn't
 * move the quote. An absolute side converts from what it quotes now, floor
 * included: 1 cNGN floored at 5 bps becomes 5 bps at 3000, not the 3.3 the
 * bare offset is worth. Bps round up to a whole number so the side is never
 * tighter than it was. With no usable mid the value is cleared: a number kept
 * across units would mean something else (1 cNGN is about 6.5 bps at 1530).
 */
export function switchUnit(
  side: Side,
  spread: Spread,
  mid: number | null,
  minSpreadBps: string,
): Spread {
  const kind = spread.kind === 'bps' ? 'abs' : 'bps'
  const value = Number(spread.value)
  if (mid === null || !(mid > 0) || spread.value.trim() === '' || !Number.isFinite(value))
    return { kind, value: '' }
  if (spread.kind === 'abs') {
    const quote = sideQuote(side, spread, mid, minSpreadBps)
    return { kind, value: quote ? String(Math.ceil(quote.bps - 1e-9)) : '' }
  }
  const price = bpsPrice(side, mid, value)
  if (!(price > 0)) return { kind, value: '' }
  const abs = side === 'buy' ? 1 / price - 1 / mid : 1 / mid - 1 / price
  return { kind, value: String(Number(abs.toPrecision(4))) }
}

/** A rate like 1530 → `1,530.00`, small rates with more digits. */
export function formatRate(rate: number): string {
  const digits = rate >= 100 ? 2 : rate >= 1 ? 4 : 8
  return rate.toLocaleString('en-US', {
    minimumFractionDigits: Math.min(digits, 2),
    maximumFractionDigits: digits,
  })
}

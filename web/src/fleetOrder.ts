// The order of the fleet list, fixed for the whole visit.
//
// Ranking by value needs every wallet read, and one slow chain can hold a read
// for seconds. Re-sorting as reads land made rows jump, and re-sorting on every
// poll made them swap whenever a price moved or a bot flapped between states.
// So the list renders in the order ranked on the previous visit, straight from
// localStorage, and never moves while the page is open. The page ranks again
// once this visit's reads are in and saves that for the next load.

import { botLabel } from './botRoutes'
import type { Bot } from './types'

const KEY = 'stitch-fleet-order'

/** What one wallet is worth: the priced sides summed, the unpriced ones named. */
export interface RowTotal {
  usd: number | null
  unpriced: string[]
}

/**
 * Three bands, then value within each:
 *   1. running (live, or waiting on Textile), richest first
 *   2. not running with money in the wallet, richest first
 *   3. not running with nothing in it
 * A value not read sorts as unknown at the bottom of its band, then by name.
 */
function compareBots(a: Bot, b: Bot, values: Record<string, RowTotal>): number {
  // An unpriced holding counts as money: it could be worth anything.
  const holdsMoney = (v: RowTotal | undefined) =>
    v !== undefined && ((v.usd !== null && v.usd > 0) || v.unpriced.length > 0)
  const band = (bot: Bot) => (bot.running ? 0 : holdsMoney(values[bot.name]) ? 1 : 2)
  const ba = band(a)
  const bb = band(b)
  if (ba !== bb) return ba - bb
  const va = values[a.name]?.usd ?? -1
  const vb = values[b.name]?.usd ?? -1
  if (va !== vb) return vb - va
  return botLabel(a).localeCompare(botLabel(b))
}

/** Bot names, ranked by band then value. The input is left as it was. */
export function rankFleet(bots: readonly Bot[], values: Record<string, RowTotal>): string[] {
  return [...bots].sort((a, b) => compareBots(a, b, values)).map((b) => b.name)
}

/**
 * The bots in a saved order. Names no longer in the fleet are skipped; bots the
 * order doesn't know (a first visit, or one added since) go last, by label, so
 * the result depends only on the bot list and the order, never on timing.
 */
export function arrangeFleet(bots: readonly Bot[], order: readonly string[]): Bot[] {
  const byName = new Map(bots.map((b) => [b.name, b]))
  const known = [...new Set(order)]
    .map((name) => byName.get(name))
    .filter((b): b is Bot => b !== undefined)
  const placed = new Set(known.map((b) => b.name))
  const added = bots
    .filter((b) => !placed.has(b.name))
    .sort((a, b) => botLabel(a).localeCompare(botLabel(b)))
  return [...known, ...added]
}

/** The order saved on the last visit, or none. Anything malformed reads as none. */
export function readFleetOrder(): string[] {
  try {
    const parsed: unknown = JSON.parse(window.localStorage.getItem(KEY) ?? '[]')
    if (!Array.isArray(parsed)) return []
    return parsed.every((n): n is string => typeof n === 'string') ? parsed : []
  } catch {
    return []
  }
}

export function saveFleetOrder(names: readonly string[]): void {
  try {
    window.localStorage.setItem(KEY, JSON.stringify(names))
  } catch {
    // Private mode or storage disabled: every visit starts alphabetical.
  }
}

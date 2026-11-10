// Narrowing the fleet list: live or not, chain, vault, and free text.
//
// The filter lives in the URL (`?live=…&chain=…&vault=…&q=…`) so opening a bot
// and pressing back lands on the same narrowed list, and a filtered view can be
// bookmarked. Everything here is pure: the page reads the params, these
// functions decide which bots show.

import { botLabel } from './botRoutes'
import { botStatus } from './botStatus'
import type { Bot } from './types'

/** Which side of "is it quoting" to show. `all` is both. */
export type LiveFilter = 'all' | 'live' | 'not-live'

/** The vault option for bots whose capital sits in the operator wallet. */
export const NO_VAULT = 'none'

export interface FleetFilter {
  live: LiveFilter
  /** Chain id as a string, or '' for every chain. */
  chain: string
  /** `chainId:vaultAddress` (lowercase), NO_VAULT, or '' for any. */
  vault: string
  /** Free text, matched against name, signer address and corridor currencies. */
  query: string
}

export const EMPTY_FILTER: FleetFilter = { live: 'all', chain: '', vault: '', query: '' }

export interface FilterOption {
  value: string
  label: string
  count: number
}

/**
 * Live means what the row's pill says: running and seated with Textile.
 * Waiting on Textile, stopped and crashed are all not live.
 */
export function isLive(bot: Bot): boolean {
  return botStatus(bot.state, bot.status, bot.config?.venue) === 'live'
}

/** The vault key a bot files under. Chain-qualified: the same address on two
 * chains is two vaults. */
export function vaultKey(bot: Bot): string {
  const config = bot.config
  if (!config?.vaultAddress) return NO_VAULT
  return `${config.chainId}:${config.vaultAddress.toLowerCase()}`
}

function networkName(bot: Bot): string | null {
  const config = bot.config
  if (!config) return null
  return config.networkLabel ?? `chain ${config.chainId}`
}

/** `0x1234…abcd`, enough to tell vaults apart in a dropdown. */
export function shortAddress(address: string): string {
  return address.length > 12 ? `${address.slice(0, 6)}…${address.slice(-4)}` : address
}

/** Everything the search box looks at, lowercased. Pairs are split into their
 * currencies too, so "cngn" finds "cNGN / USDT". */
function haystack(bot: Bot): string[] {
  const config = bot.config
  const pairs = config?.pairs ?? []
  const currencies = pairs.flatMap((p) => p.split('/').map((s) => s.trim()))
  return [
    botLabel(bot),
    bot.name,
    bot.displayName ?? '',
    config?.operatorAddress ?? '',
    config?.corridorLabel ?? '',
    ...pairs,
    ...currencies,
  ]
    .filter((s) => s !== '')
    .map((s) => s.toLowerCase())
}

/** Every word has to appear somewhere, so "cngn base" narrows rather than widens.
 * Network names count here too, so a chain can be typed as well as picked. */
export function matchesQuery(bot: Bot, query: string): boolean {
  const terms = query.toLowerCase().split(/\s+/).filter(Boolean)
  if (terms.length === 0) return true
  const fields = [...haystack(bot), (networkName(bot) ?? '').toLowerCase()]
  return terms.every((term) => fields.some((field) => field.includes(term)))
}

function matchesLive(bot: Bot, live: LiveFilter): boolean {
  if (live === 'all') return true
  return isLive(bot) === (live === 'live')
}

function matchesChain(bot: Bot, chain: string): boolean {
  return chain === '' || String(bot.config?.chainId ?? '') === chain
}

function matchesVault(bot: Bot, vault: string): boolean {
  return vault === '' || vaultKey(bot) === vault
}

/** Every filter applied, the rest skipped. Order is kept. */
export function filterFleet(bots: readonly Bot[], filter: FleetFilter): Bot[] {
  return bots.filter(
    (bot) =>
      matchesLive(bot, filter.live) &&
      matchesChain(bot, filter.chain) &&
      matchesVault(bot, filter.vault) &&
      matchesQuery(bot, filter.query),
  )
}

export function isFiltered(filter: FleetFilter): boolean {
  return (
    filter.live !== 'all' || filter.chain !== '' || filter.vault !== '' || filter.query.trim() !== ''
  )
}

function countBy(bots: readonly Bot[], key: (bot: Bot) => string | null): Map<string, number> {
  return bots.reduce((counts, bot) => {
    const k = key(bot)
    return k === null ? counts : new Map(counts).set(k, (counts.get(k) ?? 0) + 1)
  }, new Map<string, number>())
}

/** One option per chain the fleet runs on, by name. Bots with no config have no
 * chain and don't add one. */
export function chainOptions(bots: readonly Bot[]): FilterOption[] {
  const counts = countBy(bots, (b) => (b.config ? String(b.config.chainId) : null))
  const labels = new Map(
    bots.flatMap((b) => (b.config ? [[String(b.config.chainId), networkName(b) ?? '']] : [])),
  )
  return [...counts]
    .map(([value, count]) => ({ value, label: labels.get(value) ?? value, count }))
    .sort((a, b) => a.label.localeCompare(b.label))
}

/** One option per vault, plus the operator wallet when any bot quotes from it.
 * Vaults read as a short address and their chain; the wallet option goes first. */
export function vaultOptions(bots: readonly Bot[]): FilterOption[] {
  const counts = countBy(bots, vaultKey)
  const labels = new Map(
    bots
      .filter((b) => vaultKey(b) !== NO_VAULT)
      .map((b) => [vaultKey(b), `${shortAddress(b.config!.vaultAddress!)} on ${networkName(b)}`]),
  )
  const vaults = [...counts]
    .filter(([value]) => value !== NO_VAULT)
    .map(([value, count]) => ({ value, label: labels.get(value) ?? value, count }))
    .sort((a, b) => a.label.localeCompare(b.label))
  const wallet = counts.get(NO_VAULT)
  return wallet ? [{ value: NO_VAULT, label: 'No vault (operator wallet)', count: wallet }, ...vaults] : vaults
}

/** How many bots each live option would show. */
export function liveCounts(bots: readonly Bot[]): Record<LiveFilter, number> {
  const live = bots.filter(isLive).length
  return { all: bots.length, live, 'not-live': bots.length - live }
}

const LIVE_VALUES: readonly LiveFilter[] = ['all', 'live', 'not-live']

/** The filter in the URL. Anything unknown reads as unset. */
export function readFilter(params: URLSearchParams): FleetFilter {
  const live = params.get('live') ?? 'all'
  return {
    live: (LIVE_VALUES as readonly string[]).includes(live) ? (live as LiveFilter) : 'all',
    chain: params.get('chain') ?? '',
    vault: params.get('vault') ?? '',
    query: params.get('q') ?? '',
  }
}

/** The filter as URL params, leaving out whatever is at its default so a
 * clean list has a clean URL. */
export function writeFilter(filter: FleetFilter): URLSearchParams {
  const entries: [string, string][] = [
    ['live', filter.live === 'all' ? '' : filter.live],
    ['chain', filter.chain],
    ['vault', filter.vault],
    ['q', filter.query],
  ]
  return new URLSearchParams(entries.filter(([, v]) => v !== ''))
}

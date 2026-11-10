// Narrowing the fleet list: live or not, chain, vault or not, and free text.
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

/** Whether the bot quotes from an OperatorVault. `all` is both. */
export type VaultFilter = 'all' | 'linked' | 'unlinked'

export interface FleetFilter {
  live: LiveFilter
  /** Chain id as a string, or '' for every chain. */
  chain: string
  vault: VaultFilter
  /** Free text, matched against name, signer address and corridor currencies. */
  query: string
}

export const EMPTY_FILTER: FleetFilter = { live: 'all', chain: '', vault: 'all', query: '' }

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

/** Linked to a vault: `[vault]` is set, so the bot quotes the vault's money
 * rather than its own wallet's. A bot with no config has no vault. */
export function hasVault(bot: Bot): boolean {
  return !!bot.config?.vaultAddress
}

function networkName(bot: Bot): string | null {
  const config = bot.config
  if (!config) return null
  return config.networkLabel ?? `chain ${config.chainId}`
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

function matchesVault(bot: Bot, vault: VaultFilter): boolean {
  if (vault === 'all') return true
  return hasVault(bot) === (vault === 'linked')
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

/** The fewest bots worth filtering: one bot has nothing to narrow. */
export const MIN_BOTS_TO_FILTER = 2

/**
 * The filter that actually applies. Below MIN_BOTS_TO_FILTER the bar is hidden,
 * so a filter left in the URL (a bookmark, or the fleet shrank) is ignored
 * rather than hiding the one bot behind a control nobody can see.
 */
export function effectiveFilter(filter: FleetFilter, botCount: number): FleetFilter {
  return botCount < MIN_BOTS_TO_FILTER ? EMPTY_FILTER : filter
}

export function isFiltered(filter: FleetFilter): boolean {
  return (
    filter.live !== 'all' ||
    filter.chain !== '' ||
    filter.vault !== 'all' ||
    filter.query.trim() !== ''
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

/** How many bots each vault option would show. */
export function vaultCounts(bots: readonly Bot[]): Record<VaultFilter, number> {
  const linked = bots.filter(hasVault).length
  return { all: bots.length, linked, unlinked: bots.length - linked }
}

/** How many bots each live option would show. */
export function liveCounts(bots: readonly Bot[]): Record<LiveFilter, number> {
  const live = bots.filter(isLive).length
  return { all: bots.length, live, 'not-live': bots.length - live }
}

const LIVE_VALUES: readonly LiveFilter[] = ['all', 'live', 'not-live']
const VAULT_VALUES: readonly VaultFilter[] = ['all', 'linked', 'unlinked']

/** `value` when it is one of `values`, else `fallback`. */
function oneOf<T extends string>(values: readonly T[], value: string | null, fallback: T): T {
  return (values as readonly string[]).includes(value ?? '') ? (value as T) : fallback
}

/** The filter in the URL. Anything unknown reads as unset. */
export function readFilter(params: URLSearchParams): FleetFilter {
  // `vault` used to hold one vault's address; an old link like that reads as all.
  return {
    live: oneOf(LIVE_VALUES, params.get('live'), 'all'),
    chain: params.get('chain') ?? '',
    vault: oneOf(VAULT_VALUES, params.get('vault'), 'all'),
    query: params.get('q') ?? '',
  }
}

/** The filter as URL params, leaving out whatever is at its default so a
 * clean list has a clean URL. */
export function writeFilter(filter: FleetFilter): URLSearchParams {
  const entries: [string, string][] = [
    ['live', filter.live === 'all' ? '' : filter.live],
    ['chain', filter.chain],
    ['vault', filter.vault === 'all' ? '' : filter.vault],
    ['q', filter.query],
  ]
  return new URLSearchParams(entries.filter(([, v]) => v !== ''))
}

/** The URL params that name a filter, as opposed to the search text. */
const FILTER_PARAMS = ['live', 'chain', 'vault'] as const

/**
 * The part of a filter kept between visits, as a query string: live, chain
 * and vault. The search text isn't kept: a search is a one-off, and an old
 * one coming back would quietly hide bots on the next visit.
 */
export function savedForm(filter: FleetFilter): string {
  return writeFilter({ ...filter, query: '' }).toString()
}

/** Whether the URL names a filter of its own (a bookmark, a shared link, or
 * this tab's history), rather than leaving it to the saved one. */
export function hasFilterParams(params: URLSearchParams): boolean {
  return FILTER_PARAMS.some((key) => params.has(key))
}

/**
 * The filter to show: the URL's when it names one, else the one saved by the
 * last change, in this tab or another. The search text always comes from the
 * URL. `saved` is `savedForm`'s output, or null when nothing was saved.
 */
export function resolveFilter(params: URLSearchParams, saved: string | null): FleetFilter {
  const fromUrl = readFilter(params)
  if (hasFilterParams(params)) return fromUrl
  return { ...readFilter(new URLSearchParams(saved ?? '')), query: fromUrl.query }
}

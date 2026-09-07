// The pure matcher behind the wizard's corridor picker. No React and no
// fetch: the catalog goes in, and the picker asks which networks exist, which
// tokens each lists, which of them pair with a pick, and which corridor two
// tokens resolve to, in either order. Everything is keyed by chainId and by a
// case-folded symbol, never by display text, so "cNGN / USDT" on Celo and
// "cNGN / USDT" on BNB Smart Chain are two corridors and "USDT then cNGN" is
// the same pick as "cNGN then USDT".

import type { Corridor } from '../../types'
import { pairSymbols } from '../SpreadExample'

/**
 * Matching key for a symbol. The catalog is consistent today (cNGN, USDT,
 * XAUt spelled the same on every chain); the case fold is insurance against a
 * registry entry that isn't. Display keeps the first spelling seen. No alias
 * table: GD is shown as the catalog spells it.
 */
export const tokenKey = (symbol: string) => symbol.trim().toUpperCase()

/** Order-free pair key: cNGN then USDT and USDT then cNGN hit the same corridor. */
export function pairKey(chainId: number, a: string, b: string): string {
  return `${chainId}:${[tokenKey(a), tokenKey(b)].sort().join('|')}`
}

/**
 * The two symbols of a corridor, or null when the display name isn't exactly
 * two distinct symbols. Such a corridor is still offered, as a whole row.
 */
export function splitCorridor(c: Corridor): { base: string; quote: string } | null {
  const s = pairSymbols(c.displayName)
  if (!s || tokenKey(s.base) === tokenKey(s.quote)) return null
  return s
}

export interface TokenEntry {
  key: string
  /** Catalog spelling; the first one seen wins. */
  symbol: string
  /** At least one deployed corridor on this network uses the token. */
  live: boolean
}

export interface NetworkEntry {
  chainId: number
  label: string
  /** Alphabetical, case-insensitive. */
  tokens: TokenEntry[]
  /**
   * Corridors the token list can't express, offered as whole rows so nothing
   * in the catalog is unreachable: a name that isn't two symbols, or a second
   * deployed listing of a pair the list already resolves to another corridor.
   */
  extras: Corridor[]
}

export interface CorridorIndex {
  /** Catalog first-appearance order (the registry's own priority), testnets last. */
  networks: NetworkEntry[]
  /** pairKey -> corridors in catalog order. More than one only on a duplicate listing. */
  byPair: Map<string, Corridor[]>
  byId: Map<string, Corridor>
}

export function buildCorridorIndex(corridors: Corridor[]): CorridorIndex {
  type Building = NetworkEntry & { seen: Map<string, TokenEntry> }
  const networks = new Map<number, Building>()
  const byPair = new Map<string, Corridor[]>()
  const byId = new Map<string, Corridor>()
  const networkOf = (c: Corridor): Building => {
    let n = networks.get(c.chainId)
    if (!n) {
      n = {
        chainId: c.chainId,
        label: c.networkLabel,
        tokens: [],
        extras: [],
        seen: new Map(),
      }
      networks.set(c.chainId, n)
    }
    return n
  }
  const noteToken = (n: Building, symbol: string, live: boolean) => {
    const key = tokenKey(symbol)
    const t = n.seen.get(key)
    if (t) t.live = t.live || live
    else n.seen.set(key, { key, symbol: symbol.trim(), live })
  }
  for (const c of corridors) {
    byId.set(c.id, c)
    const n = networkOf(c)
    const s = splitCorridor(c)
    if (!s) {
      n.extras.push(c)
      continue
    }
    noteToken(n, s.base, !c.pendingDeploy)
    noteToken(n, s.quote, !c.pendingDeploy)
    const k = pairKey(c.chainId, s.base, s.quote)
    const twins = byPair.get(k) ?? []
    // The token list shows a pair once and resolves it to the first deployed
    // listing, so a second deployed listing is offered as a whole row instead
    // of vanishing.
    if (!c.pendingDeploy && twins.some((t) => !t.pendingDeploy)) n.extras.push(c)
    byPair.set(k, [...twins, c])
  }
  const list: NetworkEntry[] = [...networks.values()].map(({ seen, ...n }) => ({
    ...n,
    tokens: [...seen.values()].sort((a, b) =>
      a.symbol.localeCompare(b.symbol, undefined, { sensitivity: 'base' }),
    ),
  }))
  // Stable: catalog order, but a label that says testnet goes last.
  const isTest = (n: NetworkEntry) => /testnet/i.test(n.label)
  list.sort((a, b) => Number(isTest(a)) - Number(isTest(b)))
  return { networks: list, byPair, byId }
}

/** The network entry for a chain, if the catalog lists one. */
export function networkOn(index: CorridorIndex, chainId: number): NetworkEntry | undefined {
  return index.networks.find((n) => n.chainId === chainId)
}

/** The tokens listed on a chain, alphabetical. Empty for an unknown chain. */
export function tokensOn(index: CorridorIndex, chainId: number): TokenEntry[] {
  return networkOn(index, chainId)?.tokens ?? []
}

export type PartnerState = 'live' | 'pending' | 'none'

/** Whether `candidate` forms a corridor with `picked` on this network. */
export function partnerState(
  index: CorridorIndex,
  chainId: number,
  picked: string,
  candidate: string,
): PartnerState {
  if (tokenKey(picked) === tokenKey(candidate)) return 'none'
  const hits = index.byPair.get(pairKey(chainId, picked, candidate))
  if (!hits || hits.length === 0) return 'none'
  return hits.some((c) => !c.pendingDeploy) ? 'live' : 'pending'
}

/** The tokens on a chain that form a corridor with `symbol`, and whether that corridor is deployed. */
export function partnersOf(
  index: CorridorIndex,
  chainId: number,
  symbol: string,
): { token: TokenEntry; state: 'live' | 'pending' }[] {
  const out: { token: TokenEntry; state: 'live' | 'pending' }[] = []
  for (const token of tokensOn(index, chainId)) {
    const state = partnerState(index, chainId, symbol, token.symbol)
    if (state !== 'none') out.push({ token, state })
  }
  return out
}

/**
 * The corridor for two tokens on a network, whichever order they were picked
 * in. A deployed listing wins; when only a pending one exists it is returned
 * as is, flagged by its own `pendingDeploy`, so the caller can say why the
 * pick doesn't go through. Null when the pair has no corridor at all.
 */
export function resolveCorridor(
  index: CorridorIndex,
  chainId: number,
  a: string,
  b: string,
): Corridor | null {
  if (tokenKey(a) === tokenKey(b)) return null
  const hits = index.byPair.get(pairKey(chainId, a, b))
  if (!hits || hits.length === 0) return null
  return hits.find((c) => !c.pendingDeploy) ?? hits[0] ?? null
}

/**
 * Another deployed listing of this corridor's pair exists on its chain.
 *
 * The token grid shows a pair once and resolves it to the first deployed
 * listing in catalog order, so a second listing is offered as a whole row.
 * Two rows reading "cNGN / USDT" with nothing to tell them apart is a pick the
 * operator can't make on purpose: the listings can carry different feed URLs
 * (an admin-registered corridor carries `&corridor=<id>`), so the picker says
 * which is which rather than letting catalog order decide silently.
 */
export function hasTwin(index: CorridorIndex, c: Corridor): boolean {
  const s = splitCorridor(c)
  if (!s) return false
  const hits = index.byPair.get(pairKey(c.chainId, s.base, s.quote)) ?? []
  return hits.filter((x) => !x.pendingDeploy).length > 1
}

/**
 * What tells two listings of the same pair apart: the corridor id the registry
 * gave it. Unique by construction, and the same string the corridor admin
 * shows, so it is something the operator can ask about.
 */
export function listingTag(c: Corridor): string {
  return c.id
}

/**
 * Where a corridor id sits in the picker, for seeding the draft from the
 * wizard's corridorId. Undefined for '', the custom sentinel or an unknown
 * id. `tokens` is null for a corridor offered as a whole row.
 */
export function locateCorridor(
  index: CorridorIndex,
  id: string,
): { chainId: number; tokens: [string, string] | null } | undefined {
  const c = index.byId.get(id)
  if (!c) return undefined
  const whole = networkOn(index, c.chainId)?.extras.some((x) => x.id === id) ?? false
  const s = whole ? null : splitCorridor(c)
  return { chainId: c.chainId, tokens: s ? [s.base, s.quote] : null }
}

// ---------------------------------------------------------------------------
// The picker's draft: what the operator has clicked so far, and what it means.
// Pure so the component's handlers are one-liners and a node script can drive
// exactly what the UI runs.
// ---------------------------------------------------------------------------

/** A network and up to two tokens, in the order they were picked. */
export interface Draft {
  chainId: number | undefined
  picks: string[]
}

/**
 * The draft a corridor id implies: its network and, when the name splits, its
 * two tokens, so a preselected corridor shows as resolved. Anything else ('',
 * the custom sentinel, an unknown id) starts on the first network with
 * nothing picked.
 */
export function seedDraft(index: CorridorIndex, corridorId: string): Draft {
  const at = locateCorridor(index, corridorId)
  return {
    chainId: at?.chainId ?? index.networks[0]?.chainId,
    picks: at?.tokens ?? [],
  }
}

const isPicked = (draft: Draft, symbol: string) =>
  draft.picks.some((p) => tokenKey(p) === tokenKey(symbol))

/**
 * How a token row reads. Only 'picked' and 'open' take a click: 'pending' has
 * no deployed corridor (on its own, or with the first pick), 'nopair' has no
 * corridor with the first pick at all, 'full' waits for an unpick.
 */
export type RowState = 'picked' | 'open' | 'pending' | 'nopair' | 'full'

export function tokenRowState(index: CorridorIndex, draft: Draft, token: TokenEntry): RowState {
  if (isPicked(draft, token.symbol)) return 'picked'
  if (!token.live) return 'pending'
  const first = draft.picks[0]
  if (first === undefined) return 'open'
  if (draft.picks.length >= 2) return 'full'
  const s =
    draft.chainId === undefined
      ? 'none'
      : partnerState(index, draft.chainId, first, token.symbol)
  return s === 'live' ? 'open' : s === 'pending' ? 'pending' : 'nopair'
}

/** The deployed corridor id a complete draft resolves to, else ''. */
export function resolvedId(index: CorridorIndex, draft: Draft): string {
  const [a, b] = draft.picks
  if (a === undefined || b === undefined || draft.chainId === undefined) return ''
  const hit = resolveCorridor(index, draft.chainId, a, b)
  return hit && !hit.pendingDeploy ? hit.id : ''
}

/**
 * A click on a token row: unpick it when picked, else add it while there is
 * room. Returns the next draft and the corridor id it resolves to, '' while
 * the pair is incomplete.
 */
export function togglePick(
  index: CorridorIndex,
  draft: Draft,
  symbol: string,
): { draft: Draft; corridorId: string } {
  const picks = isPicked(draft, symbol)
    ? draft.picks.filter((p) => tokenKey(p) !== tokenKey(symbol))
    : draft.picks.length < 2
      ? [...draft.picks, symbol]
      : draft.picks
  const next = { chainId: draft.chainId, picks }
  return { draft: next, corridorId: resolvedId(index, next) }
}

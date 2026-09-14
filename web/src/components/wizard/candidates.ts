// Which bots on this panel could quote the corridor the operator just picked.
//
// A bot is pinned to one chain, one RPC, one reactor and one wallet, so the
// first filter is the chain: `GET /api/bots` already carries `config.chainId`
// for every bot, at no extra cost. What it does not carry is which corridors a
// bot quotes (`config.corridorId` is only its first pool, `config.pools` only a
// count), so each same-chain bot gets one `GET /settings`, which is a container
// inspect and a file read, no chain work.
//
// Everything that would be refused after the click is decided here instead, and
// a bot that fails is kept as a row with the reason on it. Silently dropping it
// leaves an operator who came to add a corridor to that exact bot with no
// explanation of where it went.

import { api } from '../../api'
import { formatAmount } from '../../format'
import { place } from './wizardCopy'
import type { Bot, Corridor, Funding, PoolSummary, Settings } from '../../types'

/**
 * Why a bot can't take the corridor, as a value rather than as prose.
 *
 * `blocked` is what the operator reads; this is what the screen branches on.
 * The two say the same thing, and only one of them can be branched on safely:
 * a screen that has to answer "your own bot already quotes this" differently
 * from "you have not connected that one yet" cannot find that out by matching
 * the sentence it printed.
 */
export type BlockedKind =
  | 'already'
  | 'not-connected'
  | 'waiting-textile'
  | 'not-editable'
  | 'unreadable'
  | 'unchecked'

export interface Candidate {
  bot: Bot
  /** Corridors this bot already quotes, as `cNGN / USDT`. */
  pairs: string[]
  operatorAddress: string | null
  /** Filled in later by a background funding read. Never blocks the screen. */
  balances: string[] | null
  eligible: boolean
  /** Why it can't take the corridor, in the operator's words. */
  blocked: string | null
  /** The same answer, for the code. Null exactly when `blocked` is null. */
  blockedKind: BlockedKind | null
  /**
   * The panel read this bot's settings, or tried to. False for a row past the
   * fan-out cap, whose reason is "not checked" rather than a refusal: a screen
   * that says none of your bots can take this corridor must not be counting
   * bots nobody looked at.
   */
  scanned: boolean
}

export interface CandidateScan {
  rows: Candidate[]
  /** Every bot the panel knows, so a prefill can tell "gone" from "elsewhere". */
  fleetNames: string[]
}

export interface TokenPair {
  /** Lowercase 0x hex. */
  collateral: string
  debt: string
}

/**
 * The corridor template's own two tokens, read the way the wizard reads its
 * other template fields: a regex over the toml it is about to send. Scoped to
 * the `[[pools]]` table so a bot-level key can't answer, and anchored on `=` so
 * `collateral_decimals` can never match.
 */
export function templatePair(toml: string): TokenPair | null {
  const at = toml.search(/^\[\[pools\]\]/m)
  const body = at >= 0 ? toml.slice(at) : toml
  const collateral = body.match(/^\s*collateral\s*=\s*"([^"]+)"/m)?.[1]
  const debt = body.match(/^\s*debt\s*=\s*"([^"]+)"/m)?.[1]
  if (!collateral || !debt) return null
  return { collateral: collateral.toLowerCase(), debt: debt.toLowerCase() }
}

/** The template's own spreads, for deciding whether the operator changed them. */
export function templateSpreads(toml: string): { buy: string | null; sell: string | null } {
  const num = (key: string) =>
    toml.match(new RegExp(`^\\s*${key}\\s*=\\s*([0-9]+(?:\\.[0-9]+)?)`, 'm'))?.[1] ?? null
  return { buy: num('buy_offset_bps'), sell: num('sell_offset_bps') }
}

/**
 * RFQ matching is order-insensitive, so a bot quoting the same market with the
 * tokens swapped is quoting the same corridor. Mirrors `same_token_pair` in
 * src/setup/settings.rs, which is what refuses the write server-side.
 */
function samePair(a: TokenPair, b: TokenPair): boolean {
  return (
    (a.collateral === b.collateral && a.debt === b.debt) ||
    (a.collateral === b.debt && a.debt === b.collateral)
  )
}

/**
 * Does this pool list already carry the corridor?
 *
 * The catalog id catches the ordinary case; the token pair catches the same
 * market listed under a second id, which the panel refuses server-side.
 */
export function poolsQuote(pools: PoolSummary[], corridor: Corridor): boolean {
  const pair = templatePair(corridor.tomlTemplate)
  return (
    pools.some((p) => p.corridorId === corridor.id) ||
    (pair !== null &&
      pools.some((p) =>
        samePair(pair, {
          collateral: p.collateral.toLowerCase(),
          debt: p.debt.toLowerCase(),
        }),
      ))
  )
}

/** The corridors one bot quotes. One container inspect and a file read. */
export async function botPools(
  bot: string,
  signal?: AbortSignal,
): Promise<PoolSummary[]> {
  return (await api.settings(bot, 0, signal)).pools
}

/**
 * How long a read the operator is actively waiting on may take before the
 * screen gives up on it.
 *
 * Neither `/api/bots` nor `/settings` carries a server-side budget, and both go
 * through Docker: a wedged daemon holds them open with no answer. That used to
 * be harmless, because the picker's Next was offline. It isn't any more, so the
 * detection gets its own clock and a timeout is treated as "no choice to
 * offer", not as an error.
 */
export const DETECT_TIMEOUT_MS = 2500

/** Run `fn` under an abort clock. Rejects like any other failed request. */
export async function withTimeout<T>(
  fn: (signal: AbortSignal) => Promise<T>,
  ms: number = DETECT_TIMEOUT_MS,
): Promise<T> {
  const control = new AbortController()
  const timer = window.setTimeout(() => control.abort(), ms)
  try {
    return await fn(control.signal)
  } finally {
    window.clearTimeout(timer)
  }
}

/** Wallet balances as prose: `1,240 USDT`. Rows the chain read missed are left out. */
export function balanceLine(funding: Funding): string[] {
  return funding.tokens
    .filter((t) => t.balanceText !== null)
    .map((t) => `${formatAmount(t.balanceText ?? '0')} ${t.symbol}`)
}

/**
 * How many same-chain bots get a settings read on one click.
 *
 * Each is a container inspect and a file read, fired in parallel, and this now
 * runs on the picker's Next where nothing used to run at all. High enough that
 * a real fleet is read whole (nobody runs eight stitches on one network), low
 * enough that it can't become a burst. The bot the operator actually came for
 * is read first, so the cap can never answer their own question with silence.
 */
const MAX_SETTINGS_READS = 8

/**
 * Every bot on the corridor's chain, eligible or not.
 *
 * `exempt` is the bot a resumed run already added this corridor to: on that one
 * pass the duplicate check would be reporting the flow's own work back at it.
 * `prefer` is the bot the operator came here for, from a Fleet row: it is read
 * first, so a large fleet can never push it past the cap and answer the one
 * question they asked with "not checked".
 */
export async function loadCandidates(
  corridor: Corridor,
  exempt: string | null = null,
  opts: { prefer?: string | null; signal?: AbortSignal } = {},
): Promise<CandidateScan> {
  const { prefer = null, signal } = opts
  const fleet = await api.fleet(signal)
  const fleetNames = fleet.bots.map((b) => b.name)
  const onChain = fleet.bots.filter((b) => b.config?.chainId === corridor.chainId)
  if (onChain.length === 0) return { rows: [], fleetNames }

  const first = (bot: Bot) => (bot.name === prefer || bot.name === exempt ? 0 : 1)
  const ordered = [...onChain].sort((a, b) => first(a) - first(b))
  const read = ordered.slice(0, MAX_SETTINGS_READS)
  // One unreadable bot must not take the screen down with it: it becomes a row
  // that says so, like every other bot that can't take the corridor.
  const settled = await Promise.allSettled(
    read.map((bot) => api.settings(bot.name, 0, signal)),
  )
  const rows = ordered.map((bot, i) => {
    if (i >= read.length) return unchecked(bot)
    const answer = settled[i]
    return answer && answer.status === 'fulfilled'
      ? describe(bot, answer.value, corridor, exempt)
      : unreadable(bot)
  })
  rows.sort(compare)
  return { rows, fleetNames }
}

function unreadable(bot: Bot): Candidate {
  return blank(bot, place.blockedUnreadable, 'unreadable', true)
}

/** Past the fan-out cap. Never silently dropped: it is a row with its reason. */
function unchecked(bot: Bot): Candidate {
  return blank(bot, place.blockedUnchecked, 'unchecked', false)
}

function blank(
  bot: Bot,
  blocked: string,
  blockedKind: BlockedKind,
  scanned: boolean,
): Candidate {
  return {
    bot,
    pairs: [],
    operatorAddress: bot.config?.operatorAddress ?? null,
    balances: null,
    eligible: false,
    blocked,
    blockedKind,
    scanned,
  }
}

function describe(
  bot: Bot,
  settings: Settings,
  corridor: Corridor,
  exempt: string | null,
): Candidate {
  const quotesIt = poolsQuote(settings.pools, corridor)

  let blocked: string | null = null
  let blockedKind: BlockedKind | null = null
  if (quotesIt && bot.name !== exempt) {
    blocked = place.blockedAlready(corridor.displayName)
    blockedKind = 'already'
  }
  // A key on disk with RFQ still off is a maker Textile has not answered yet,
  // not a setup nobody finished. Telling that operator to go and connect sends
  // them to a screen with nothing left to press.
  else if (!settings.rfqEnabled) {
    blocked = settings.rfqApiKeySet
      ? place.blockedWaitingTextile
      : place.blockedNotConnected
    blockedKind = settings.rfqApiKeySet ? 'waiting-textile' : 'not-connected'
  } else if (!bot.editable || !settings.editable) {
    blocked = place.blockedNotEditable
    blockedKind = 'not-editable'
  }

  return {
    bot,
    pairs: settings.pools.map((p) => p.pair),
    operatorAddress: bot.config?.operatorAddress ?? null,
    balances: null,
    eligible: blocked === null,
    blocked,
    blockedKind,
    scanned: true,
  }
}

/** Eligible first, then a running bot over a stopped one, then by name. */
function compare(a: Candidate, b: Candidate): number {
  if (a.eligible !== b.eligible) return a.eligible ? -1 : 1
  if (a.bot.running !== b.bot.running) return a.bot.running ? -1 : 1
  return a.bot.name.localeCompare(b.bot.name)
}

/**
 * Can this bot quote the new corridor at all?
 *
 * Deliberately not `funding.gate.passes`: that is already true on any bot whose
 * sibling corridor holds USDT, which is nearly all of them, so it would wave
 * through a corridor with nothing behind either side. This asks the narrower
 * question the new corridor actually turns on: one of ITS two tokens funded,
 * plus gas. `funded === null` is a failed price read, not a yes.
 */
export function pairFunded(funding: Funding, pair: TokenPair | null): boolean {
  if (funding.gas.ok === false) return false
  // `gate.passes` is gas-only now (Approve asks for gas, Live waits for
  // money), so it no longer says anything about tokens. Ask the rows.
  if (!pair) return funding.gate.fundedTokens.length > 0
  const row = (address: string) =>
    funding.tokens.find((t) => t.token.toLowerCase() === address)
  return row(pair.collateral)?.funded === true || row(pair.debt)?.funded === true
}

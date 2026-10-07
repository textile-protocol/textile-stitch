// What the wallet holds, as one number, and how to keep reading it.
//
// The bot page's header and its Funds tab show the same figure from the same
// read, so the rule for it lives here once: priced sides summed, unpriced
// sides named, and nothing at all when nothing could be priced, rather than a
// total that quietly reads low. Fleet rows apply the same rule to the capital
// alone, which for a vault maker is the vault and not its signing key.

import { useCallback, useEffect, useState } from 'react'
import { api } from './api'
import { poll } from './poll'
import type { Funding, FundingToken } from './types'

/** Five seconds, like the wizard: money arrives while you watch. */
export const FUNDING_POLL_MS = 5000

/**
 * Every corridor token row the read produced: the capital's, plus what the
 * signer wallet itself holds when that is somewhere else. Two addresses, never
 * the same money twice.
 */
function allTokens(funding: Funding): FundingToken[] {
  return [...funding.tokens, ...(funding.walletTokens ?? [])]
}

/**
 * What the signer wallet still holds of the corridor tokens, when the capital
 * sits in a vault. Empty without one, and empty when the wallet is clean —
 * which is the normal state: a vault maker's money is the vault's, and only a
 * mistaken transfer puts a corridor token on the signing key.
 */
export function walletDust(funding: Funding | null): FundingToken[] {
  return (funding?.walletTokens ?? []).filter(
    (t) => t.balance !== null && t.balance !== '0',
  )
}

/** One priced or unpriced holding: a token row or the gas coin. */
type Holding = Pick<FundingToken, 'symbol' | 'balance' | 'usd'>

/** Everything the read produced: both addresses' tokens and the signer's gas. */
function everything(funding: Funding): Holding[] {
  return [...allTokens(funding), funding.gas]
}

/**
 * What the bot quotes against. With a vault that is the vault's quotable
 * inventory and nothing else: the signer wallet's gas and dust pay for
 * transactions, they are not liquidity. Without a vault the wallet is the
 * capital, gas included.
 */
function capital(funding: Funding): Holding[] {
  return funding.capitalSource === 'vault' ? funding.tokens : everything(funding)
}

/** The priced holdings summed, or null when none could be priced. */
function sumUsd(holdings: Holding[]): number | null {
  const priced = holdings.map((h) => h.usd).filter((u): u is number => u !== null)
  if (priced.length === 0) return null
  return priced.reduce((a, b) => a + b, 0)
}

/** Symbols with a balance that could be read but not priced. The same token
 * can come back unpriced at both addresses; it is one missing price to
 * report, not two. */
function unpricedIn(holdings: Holding[]): string[] {
  return [
    ...new Set(
      holdings
        .filter((h) => h.usd === null && h.balance !== null && h.balance !== '0')
        .map((h) => h.symbol),
    ),
  ]
}

/** Dollars the bot has, or null when nothing could be priced. */
export function totalUsd(funding: Funding | null): number | null {
  return funding ? sumUsd(everything(funding)) : null
}

/** Symbols with a balance the panel could read but not price, the gas coin
 * included: a custom chain's coin with no dollar price is still money. */
export function unpricedSymbols(funding: Funding | null): string[] {
  return funding ? unpricedIn(everything(funding)) : []
}

/** Dollars of capital the bot quotes against: the vault's when there is one,
 * else the same as `totalUsd`. Null when nothing could be priced. */
export function capitalUsd(funding: Funding | null): number | null {
  return funding ? sumUsd(capital(funding)) : null
}

/** `unpricedSymbols`, for the capital `capitalUsd` sums. */
export function capitalUnpricedSymbols(funding: Funding | null): string[] {
  return funding ? unpricedIn(capital(funding)) : []
}

/**
 * Poll a bot's wallet. The value is null until the first read lands and
 * again the moment `name` changes, so a page that switches bots never shows
 * one bot's money under another's title; a read that comes back after the
 * switch is dropped. `null` as the name turns the poll off.
 */
export function useFunding(name: string | null): {
  funding: Funding | null
  refresh: () => void
} {
  const [funding, setFunding] = useState<Funding | null>(null)
  const [tick, setTick] = useState(0)
  const refresh = useCallback(() => setTick((n) => n + 1), [])

  // Reset on a bot change only, not on `refresh`: a re-read after a withdraw
  // should not blank the header for a tick.
  useEffect(() => setFunding(null), [name])

  useEffect(() => {
    if (!name) return
    let cancelled = false
    const read = () =>
      api
        .funding(name)
        .then((f) => {
          if (!cancelled) setFunding(f)
        })
        .catch(() => {
          // The header shows a dash and the Funds tab says what it can; the
          // next tick tries again.
        })
    const stop = poll(read, FUNDING_POLL_MS, { immediate: true })
    return () => {
      cancelled = true
      stop()
    }
  }, [name, tick])

  return { funding, refresh }
}

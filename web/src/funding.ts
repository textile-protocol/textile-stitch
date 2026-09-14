// What the wallet holds, as one number, and how to keep reading it.
//
// The bot page's header, its Funds tab and every fleet row show the same
// figure from the same read, so the rule for it lives here once: priced
// sides summed, unpriced sides named, and nothing at all when nothing could
// be priced, rather than a total that quietly reads low.

import { useCallback, useEffect, useState } from 'react'
import { api } from './api'
import type { Funding } from './types'

/** Five seconds, like the wizard: money arrives while you watch. */
export const FUNDING_POLL_MS = 5000

/** Dollars in the wallet, or null when nothing could be priced. */
export function totalUsd(funding: Funding | null): number | null {
  if (!funding) return null
  const priced = [...funding.tokens.map((t) => t.usd), funding.gas.usd].filter(
    (u): u is number => u !== null,
  )
  if (priced.length === 0) return null
  return priced.reduce((a, b) => a + b, 0)
}

/** Symbols with a balance the panel could read but not price, the gas coin
 * included: a custom chain's coin with no dollar price is still money. */
export function unpricedSymbols(funding: Funding | null): string[] {
  if (!funding) return []
  const held = (usd: number | null, balance: string | null) =>
    usd === null && balance !== null && balance !== '0'
  return [
    ...funding.tokens.filter((t) => held(t.usd, t.balance)).map((t) => t.symbol),
    ...(held(funding.gas.usd, funding.gas.balance) ? [funding.gas.symbol] : []),
  ]
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
      void api
        .funding(name)
        .then((f) => {
          if (!cancelled) setFunding(f)
        })
        .catch(() => {
          // The header shows a dash and the Funds tab says what it can; the
          // next tick tries again.
        })
    read()
    const timer = window.setInterval(read, FUNDING_POLL_MS)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [name, tick])

  return { funding, refresh }
}

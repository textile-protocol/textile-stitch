import { describe, expect, it } from 'vitest'
import { capitalUnpricedSymbols, capitalUsd, totalUsd, unpricedSymbols } from './funding'
import type { Funding, FundingToken } from './types'

// Only the fields the totals read; the rest of a Funding doesn't matter here.
const token = (symbol: string, usd: number | null, balance: string | null = '1'): FundingToken =>
  ({ symbol, usd, balance }) as FundingToken

const funding = (f: {
  source: 'vault' | 'wallet'
  tokens: FundingToken[]
  walletTokens?: FundingToken[] | null
  gasUsd?: number | null
  gasBalance?: string | null
}): Funding =>
  ({
    capitalSource: f.source,
    tokens: f.tokens,
    walletTokens: f.walletTokens ?? null,
    gas: { symbol: 'CELO', usd: f.gasUsd ?? null, balance: f.gasBalance ?? '0' },
  }) as Funding

describe('capitalUsd', () => {
  it("counts only the vault's inventory for a vault maker", () => {
    const f = funding({
      source: 'vault',
      tokens: [token('USDT', 1000), token('cNGN', 250)],
      walletTokens: [token('USDT', 3)],
      gasUsd: 5,
      gasBalance: '1',
    })
    expect(capitalUsd(f)).toBe(1250)
    // The bot page's total still counts the signer wallet too.
    expect(totalUsd(f)).toBe(1258)
  })

  it('reads null when the vault could not be priced, not the gas on the key', () => {
    const f = funding({
      source: 'vault',
      tokens: [token('USDT', null, null)],
      walletTokens: [token('USDT', 0, '0')],
      gasUsd: 5,
      gasBalance: '1',
    })
    expect(capitalUsd(f)).toBeNull()
  })

  it('is the whole wallet, gas included, without a vault', () => {
    const f = funding({ source: 'wallet', tokens: [token('USDT', 100)], gasUsd: 2, gasBalance: '1' })
    expect(capitalUsd(f)).toBe(102)
    expect(capitalUsd(f)).toBe(totalUsd(f))
  })

  it('is null with no read', () => {
    expect(capitalUsd(null)).toBeNull()
  })
})

describe('capitalUnpricedSymbols', () => {
  it("names only the vault's unpriced tokens for a vault maker", () => {
    const f = funding({
      source: 'vault',
      tokens: [token('XYZ', null)],
      walletTokens: [token('ABC', null)],
      gasUsd: null,
      gasBalance: '1',
    })
    expect(capitalUnpricedSymbols(f)).toEqual(['XYZ'])
    expect(unpricedSymbols(f)).toEqual(['XYZ', 'ABC', 'CELO'])
  })

  it('reports a token unpriced at both addresses once', () => {
    const f = funding({
      source: 'vault',
      tokens: [token('XYZ', null)],
      walletTokens: [token('XYZ', null)],
    })
    expect(unpricedSymbols(f)).toEqual(['XYZ'])
  })
})

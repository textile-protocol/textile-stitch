import { describe, expect, it } from 'vitest'
import {
  EMPTY_FILTER,
  chainOptions,
  effectiveFilter,
  filterFleet,
  isFiltered,
  liveCounts,
  matchesQuery,
  readFilter,
  resolveFilter,
  savedForm,
  vaultCounts,
  writeFilter,
  type FleetFilter,
} from './fleetFilter'
import type { Bot, BotState, ConfigBody } from './types'

const VAULT = '0xAbCdEf0000000000000000000000000000001234'
const OPERATOR = '0x9f3c000000000000000000000000000000007e21'

// Only the fields the filter reads; the rest of a Bot doesn't matter here.
function bot(
  name: string,
  opts: {
    state?: BotState
    status?: string
    chainId?: number
    networkLabel?: string | null
    pairs?: string[]
    vaultAddress?: string | null
    operatorAddress?: string | null
    venue?: ConfigBody['venue']
    displayName?: string | null
    noConfig?: boolean
  } = {},
): Bot {
  const config = opts.noConfig
    ? null
    : ({
        chainId: opts.chainId ?? 8453,
        networkLabel: opts.networkLabel === undefined ? 'Base' : opts.networkLabel,
        pairs: opts.pairs ?? ['cNGN / USDT'],
        vaultAddress: opts.vaultAddress ?? null,
        operatorAddress: opts.operatorAddress ?? null,
        corridorLabel: null,
        venue: opts.venue ?? 'seated',
      } as ConfigBody)
  return {
    name,
    displayName: opts.displayName ?? null,
    state: opts.state ?? 'running',
    status: opts.status ?? 'Up 2 hours',
    config,
  } as Bot
}

const names = (bots: Bot[]) => bots.map((b) => b.name)
const with_ = (patch: Partial<FleetFilter>): FleetFilter => ({ ...EMPTY_FILTER, ...patch })

describe('filterFleet: live', () => {
  const fleet = [
    bot('live'),
    bot('waiting', { venue: 'waiting' }),
    bot('stopped', { state: 'exited', status: 'Exited (0) 1 hour ago' }),
    bot('crashed', { state: 'exited', status: 'Exited (137) 1 hour ago' }),
  ]

  it('shows both by default', () => {
    expect(names(filterFleet(fleet, EMPTY_FILTER))).toEqual(['live', 'waiting', 'stopped', 'crashed'])
  })

  it('live means the pill says live: running and seated', () => {
    expect(names(filterFleet(fleet, with_({ live: 'live' })))).toEqual(['live'])
  })

  it('not live takes waiting, stopped and crashed', () => {
    expect(names(filterFleet(fleet, with_({ live: 'not-live' })))).toEqual([
      'waiting',
      'stopped',
      'crashed',
    ])
  })

  it('counts each side', () => {
    expect(liveCounts(fleet)).toEqual({ all: 4, live: 1, 'not-live': 3 })
  })
})

describe('filterFleet: chain and vault', () => {
  const fleet = [
    bot('base-wallet'),
    bot('base-vault', { vaultAddress: VAULT }),
    bot('celo-vault', { chainId: 42220, networkLabel: 'Celo', vaultAddress: VAULT }),
    bot('bare', { noConfig: true }),
  ]

  it('keeps one chain', () => {
    expect(names(filterFleet(fleet, with_({ chain: '42220' })))).toEqual(['celo-vault'])
  })

  it('with vault keeps every bot linked to one, on any chain', () => {
    expect(names(filterFleet(fleet, with_({ vault: 'linked' })))).toEqual([
      'base-vault',
      'celo-vault',
    ])
  })

  it('without vault keeps the rest, a bot with no config included', () => {
    expect(names(filterFleet(fleet, with_({ vault: 'unlinked' })))).toEqual(['base-wallet', 'bare'])
  })

  it('combines filters', () => {
    expect(names(filterFleet(fleet, with_({ chain: '8453', vault: 'unlinked' })))).toEqual([
      'base-wallet',
    ])
  })

  it('counts each vault option', () => {
    expect(vaultCounts(fleet)).toEqual({ all: 4, linked: 2, unlinked: 2 })
  })

  it('lists chains by name with counts, skipping bots with no config', () => {
    expect(chainOptions(fleet)).toEqual([
      { value: '8453', label: 'Base', count: 2 },
      { value: '42220', label: 'Celo', count: 1 },
    ])
  })

  it('falls back to the chain id when the network has no name', () => {
    expect(chainOptions([bot('x', { chainId: 999, networkLabel: null })])).toEqual([
      { value: '999', label: 'chain 999', count: 1 },
    ])
  })
})

describe('matchesQuery', () => {
  const b = bot('lagos-1', {
    displayName: 'Lagos desk',
    operatorAddress: OPERATOR,
    pairs: ['cNGN / USDT', 'wBRL / USDT'],
  })

  it('matches nothing typed', () => {
    expect(matchesQuery(b, '  ')).toBe(true)
  })

  it('matches the display name and the id, any case', () => {
    expect(matchesQuery(b, 'LAGOS DESK')).toBe(true)
    expect(matchesQuery(b, 'lagos-1')).toBe(true)
  })

  it('matches part of the signer address, any case', () => {
    expect(matchesQuery(b, '0x9F3C')).toBe(true)
    expect(matchesQuery(b, '7e21')).toBe(true)
    expect(matchesQuery(b, '0xdead')).toBe(false)
  })

  it('matches a currency on any corridor', () => {
    expect(matchesQuery(b, 'wbrl')).toBe(true)
    expect(matchesQuery(b, 'zarp')).toBe(false)
  })

  it('needs every word to match somewhere', () => {
    expect(matchesQuery(b, 'cngn base')).toBe(true)
    expect(matchesQuery(b, 'cngn celo')).toBe(false)
  })
})

describe('URL round trip', () => {
  it('keeps a clean URL for the default filter', () => {
    expect(writeFilter(EMPTY_FILTER).toString()).toBe('')
    expect(isFiltered(EMPTY_FILTER)).toBe(false)
  })

  it('reads back what it wrote', () => {
    const f: FleetFilter = { live: 'not-live', chain: '8453', vault: 'unlinked', query: 'cngn' }
    expect(readFilter(writeFilter(f))).toEqual(f)
    expect(isFiltered(f)).toBe(true)
  })

  it('reads an unknown live value as both', () => {
    expect(readFilter(new URLSearchParams('live=maybe')).live).toBe('all')
  })

  it('reads an old single-vault link as all, rather than filtering on nothing', () => {
    expect(readFilter(new URLSearchParams(`vault=8453:${VAULT.toLowerCase()}`)).vault).toBe('all')
  })
})

describe('effectiveFilter', () => {
  const f: FleetFilter = { live: 'live', chain: '8453', vault: 'unlinked', query: 'x' }

  it('ignores the filter for a lone bot, so it never hides behind a hidden bar', () => {
    expect(effectiveFilter(f, 1)).toEqual(EMPTY_FILTER)
    expect(effectiveFilter(f, 0)).toEqual(EMPTY_FILTER)
    const lone = [bot('idle', { state: 'exited', status: 'Exited (0) 1 hour ago' })]
    expect(names(filterFleet(lone, effectiveFilter(with_({ live: 'live' }), lone.length)))).toEqual([
      'idle',
    ])
  })

  it('applies it from two bots up', () => {
    expect(effectiveFilter(f, 2)).toBe(f)
  })
})

describe('saved filter', () => {
  const saved = savedForm({ live: 'live', chain: '8453', vault: 'linked', query: 'cngn' })
  const params = (s: string) => new URLSearchParams(s)

  it('keeps live, chain and vault, not the search text', () => {
    expect(saved).toBe('live=live&chain=8453&vault=linked')
    expect(savedForm(EMPTY_FILTER)).toBe('')
  })

  it('comes back on a visit whose URL names no filter', () => {
    expect(resolveFilter(params(''), saved)).toEqual({
      live: 'live',
      chain: '8453',
      vault: 'linked',
      query: '',
    })
  })

  it('keeps the search text from the URL when it does', () => {
    expect(resolveFilter(params('q=lagos'), saved).query).toBe('lagos')
    expect(resolveFilter(params('q=lagos'), saved).live).toBe('live')
  })

  it('gives way to a URL that names a filter of its own', () => {
    expect(resolveFilter(params('vault=unlinked'), saved)).toEqual({
      ...EMPTY_FILTER,
      vault: 'unlinked',
    })
  })

  it('reads nothing saved, or junk, as no filter', () => {
    expect(resolveFilter(params(''), null)).toEqual(EMPTY_FILTER)
    expect(resolveFilter(params(''), 'live=maybe&vault=0xabc')).toEqual(EMPTY_FILTER)
  })
})

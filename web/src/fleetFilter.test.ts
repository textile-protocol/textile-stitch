import { describe, expect, it } from 'vitest'
import {
  EMPTY_FILTER,
  NO_VAULT,
  chainOptions,
  effectiveFilter,
  filterFleet,
  isFiltered,
  liveCounts,
  matchesQuery,
  readFilter,
  vaultOptions,
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

  it('treats the same address on two chains as two vaults', () => {
    expect(names(filterFleet(fleet, with_({ vault: `8453:${VAULT.toLowerCase()}` })))).toEqual([
      'base-vault',
    ])
  })

  it('no vault means the operator wallet funds it', () => {
    expect(names(filterFleet(fleet, with_({ vault: NO_VAULT })))).toEqual(['base-wallet', 'bare'])
  })

  it('combines filters', () => {
    expect(names(filterFleet(fleet, with_({ chain: '8453', vault: NO_VAULT })))).toEqual([
      'base-wallet',
    ])
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

  it('lists the operator wallet first, then each vault with its chain', () => {
    expect(vaultOptions(fleet)).toEqual([
      { value: NO_VAULT, label: 'No vault (operator wallet)', count: 2 },
      { value: `8453:${VAULT.toLowerCase()}`, label: '0xAbCd…1234 on Base', count: 1 },
      { value: `42220:${VAULT.toLowerCase()}`, label: '0xAbCd…1234 on Celo', count: 1 },
    ])
  })

  it('leaves out the wallet option when every bot has a vault', () => {
    expect(vaultOptions([bot('v', { vaultAddress: VAULT })]).map((o) => o.value)).toEqual([
      `8453:${VAULT.toLowerCase()}`,
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
    const f: FleetFilter = { live: 'not-live', chain: '8453', vault: NO_VAULT, query: 'cngn' }
    expect(readFilter(writeFilter(f))).toEqual(f)
    expect(isFiltered(f)).toBe(true)
  })

  it('reads an unknown live value as both', () => {
    expect(readFilter(new URLSearchParams('live=maybe')).live).toBe('all')
  })
})

describe('effectiveFilter', () => {
  const f: FleetFilter = { live: 'live', chain: '8453', vault: NO_VAULT, query: 'x' }

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

import { describe, expect, it } from 'vitest'
import {
  agentIdFromOutput,
  confirmRegisterText,
  gasPhrase,
  identityPhase,
  networkName,
  normalizeText,
  profileProblem,
  registrationFile,
} from './agentIdentity'
import type { AgentIdentity } from './types'

const base: AgentIdentity = {
  chainId: 42220,
  networkLabel: 'Celo',
  registry: '0x8004A169FB4a3325136EB29fA0ceB6D2e539a432',
  owner: '0x3d17BEE70baf9EC50f6f35ea73CfF0feA290D409',
  agentId: null,
  stored: null,
  ownedBy: null,
  defaults: {
    name: 'Acme MM · cNGN/USDT',
    description: 'Market-making bot quoting cNGN/USDT on Textile FX (Celo), running Stitch.',
  },
  limits: { name: 64, description: 280 },
  ownedElsewhere: false,
  sibling: null,
  links: { agent: null, registry: null },
  gas: { estimate: '0.09391', balance: '12.41', symbol: 'CELO', enough: true },
  viaCustody: false,
  pending: null,
  canRegister: true,
  blockedReason: null,
  blockedKind: null,
  blockedBy: null,
  readError: null,
}

describe('identityPhase', () => {
  it('is unavailable on a chain without a registry, whatever else is set', () => {
    expect(identityPhase({ ...base, registry: null, agentId: '7' })).toBe('unavailable')
  })

  it('is registered once the config names an id', () => {
    expect(identityPhase({ ...base, agentId: '0', canRegister: false })).toBe('registered')
  })

  it('checks on a pending Fireblocks registration before anything else', () => {
    const pending = { message: 'waiting' }
    expect(identityPhase({ ...base, pending, ownedElsewhere: true })).toBe('pending')
    expect(identityPhase({ ...base, pending, agentId: '3' })).toBe('registered')
  })

  it('links rather than mints when the wallet already owns one', () => {
    expect(identityPhase({ ...base, ownedElsewhere: true, canRegister: false })).toBe('link')
    expect(identityPhase(base)).toBe('form')
  })
})

describe('networkName and gasPhrase', () => {
  it('fall back sensibly', () => {
    expect(networkName({ networkLabel: null, chainId: 146 })).toBe('chain 146')
    expect(networkName(base)).toBe('Celo')
    expect(gasPhrase(null)).toBeNull()
    expect(gasPhrase(base.gas)).toBe('about 0.09391 CELO')
  })
})

describe('profileProblem', () => {
  it('counts characters after normalizing, like the server', () => {
    expect(profileProblem({ name: '  ', description: 'x' }, base.limits)).toBe('Give it a name.')
    expect(profileProblem({ name: 'x', description: '\n' }, base.limits)).toBe(
      'Give it a description.',
    )
    expect(profileProblem({ name: 'é'.repeat(64), description: 'x' }, base.limits)).toBeNull()
    expect(profileProblem({ name: 'n'.repeat(65), description: 'x' }, base.limits)).toBe(
      'Keep the name to 64 characters.',
    )
    expect(normalizeText('  Acme\n  MM ')).toBe('Acme MM')
  })
})

describe('registrationFile', () => {
  it('is byte for byte what stitch register stores', () => {
    // Same literal as `the_stored_json_is_compact_and_in_key_order` in src/chain/erc8004.rs.
    expect(registrationFile(base.defaults!, 42220, null)).toBe(
      '{"type":"https://eips.ethereum.org/EIPS/eip-8004#registration-v1",' +
        '"name":"Acme MM · cNGN/USDT",' +
        '"description":"Market-making bot quoting cNGN/USDT on Textile FX (Celo), running Stitch.",' +
        '"services":[{"name":"web","endpoint":"https://app.textilecredit.com"}],' +
        '"active":true,"supportedTrust":["reputation"]}',
    )
  })

  it('adds the vault as a wallet service', () => {
    const file = JSON.parse(registrationFile(base.defaults!, 42220, '0xVault'))
    expect(file.services[1]).toEqual({ name: 'wallet', endpoint: 'eip155:42220:0xVault' })
  })
})

describe('agentIdFromOutput', () => {
  it('takes the last agent_id line and nothing that merely mentions one', () => {
    const lines = [
      { text: 'registered as agent #1847 on Celo in tx 0xabc' },
      { text: 'agent_id=1847' },
    ]
    expect(agentIdFromOutput(lines)).toBe('1847')
    expect(agentIdFromOutput([{ text: 'agent_id=0' }])).toBe('0')
    expect(agentIdFromOutput([{ text: 'registered as agent #1847' }])).toBeNull()
    expect(agentIdFromOutput([{ text: 'agent_id=12x' }])).toBeNull()
  })
})

describe('confirmRegisterText', () => {
  it('names the wallet, the network, the cost and that the text is public', () => {
    const text = confirmRegisterText('cngn-celo', base)
    expect(text).toContain('on Celo?')
    expect(text).toContain('from 0x3d17…D409 and costs about 0.09391 CELO.')
    expect(text).toContain('stored onchain where anyone can read them')
    expect(text).not.toContain('Fireblocks')
  })

  it('says a custody signer sends it as a Contract Call', () => {
    expect(confirmRegisterText('fb', { ...base, viaCustody: true })).toContain(
      'Fireblocks sends it as a Contract Call.',
    )
  })
})

// What the ERC-8004 card shows, decided from the panel's identity read, plus
// the registration file exactly as `stitch register` stores it, so the
// preview is the bytes that go onchain. Pure, so the rules are testable
// without rendering anything.

import { shortAddress } from './format'
import type { AgentIdentity, AgentProfile } from './types'

export type IdentityPhase =
  /** No registry on this chain: nothing to offer. */
  | 'unavailable'
  /** The config names an agent id. */
  | 'registered'
  /** A Fireblocks registration is out: check on it, never send another. */
  | 'pending'
  /** The wallet already owns an identity the config doesn't name: link it. */
  | 'link'
  /** Nothing yet: edit the text and register. */
  | 'form'

export function identityPhase(identity: AgentIdentity): IdentityPhase {
  if (!identity.registry) return 'unavailable'
  if (identity.agentId) return 'registered'
  if (identity.pending) return 'pending'
  return identity.ownedElsewhere ? 'link' : 'form'
}

/** The network as a sentence fragment: "Celo", or "chain 146" for a custom one. */
export function networkName(identity: Pick<AgentIdentity, 'networkLabel' | 'chainId'>): string {
  return identity.networkLabel ?? `chain ${identity.chainId}`
}

/** "about 0.094 CELO", or null when the panel couldn't price the mint. */
export function gasPhrase(gas: AgentIdentity['gas']): string | null {
  return gas ? `about ${gas.estimate} ${gas.symbol}` : null
}

/**
 * The same normalization the server applies before storing: trim, and collapse
 * every run of whitespace to one space.
 */
export function normalizeText(text: string): string {
  return text.trim().split(/\s+/).filter(Boolean).join(' ')
}

/** Why the text can't be stored as typed, or null when it can. */
export function profileProblem(
  profile: AgentProfile,
  limits: AgentIdentity['limits'],
): string | null {
  const name = [...normalizeText(profile.name)].length
  const description = [...normalizeText(profile.description)].length
  if (name === 0) return 'Give it a name.'
  if (description === 0) return 'Give it a description.'
  if (name > limits.name) return `Keep the name to ${limits.name} characters.`
  if (description > limits.description)
    return `Keep the description to ${limits.description} characters.`
  return null
}

/** Fixed parts of the file. Must match `src/chain/erc8004.rs`. */
const REGISTRATION_TYPE = 'https://eips.ethereum.org/EIPS/eip-8004#registration-v1'
const WEB_ENDPOINT = 'https://app.textilecredit.com'

/**
 * The registration file `stitch register` stores for this text, byte for byte:
 * same keys, same order, compact JSON. A vault maker lists its vault as a
 * `wallet` service. `agentIdentity.test.ts` pins this to the Rust output.
 */
export function registrationFile(
  profile: AgentProfile,
  chainId: number,
  vault: string | null,
): string {
  const services: Array<{ name: string; endpoint: string }> = [
    { name: 'web', endpoint: WEB_ENDPOINT },
  ]
  if (vault) services.push({ name: 'wallet', endpoint: `eip155:${chainId}:${vault}` })
  return JSON.stringify({
    type: REGISTRATION_TYPE,
    name: normalizeText(profile.name),
    description: normalizeText(profile.description),
    services,
    active: true,
    supportedTrust: ['reputation'],
  })
}

/** The file pretty-printed for reading. Storage is the compact form. */
export function readableFile(file: string): string {
  return JSON.stringify(JSON.parse(file), null, 2)
}

/** The id `stitch register` prints as its last line, `agent_id=<id>`. */
export function agentIdFromOutput(lines: ReadonlyArray<{ text: string }>): string | null {
  const last = [...lines]
    .reverse()
    .map((line) => /^agent_id=(\d+)\s*$/.exec(line.text.trim()))
    .find((match) => match !== null)
  return last?.[1] ?? null
}

/** The confirm-dialog text before a mint: one transaction, from whom, for how much. */
export function confirmRegisterText(bot: string, identity: AgentIdentity): string {
  const from = identity.owner ? shortAddress(identity.owner) : 'the operator wallet'
  const cost = gasPhrase(identity.gas)
  const via = identity.viaCustody ? ' Fireblocks sends it as a Contract Call.' : ''
  return [
    `Register ${bot} as an ERC-8004 agent on ${networkName(identity)}?`,
    '',
    `This sends one transaction from ${from}${cost ? ` and costs ${cost}` : ''}.${via} ` +
      'That wallet becomes the owner of the identity, and the name and description ' +
      'are stored onchain where anyone can read them.',
  ].join('\n')
}

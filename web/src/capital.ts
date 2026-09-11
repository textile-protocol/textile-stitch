// Where a bot's trading capital sits — which is not the same question as which
// key signs for it.
//
// A vault maker's orders name the OperatorVault as swapper and recipient: the
// bot key is the strategy signer only. So the chain, and anything indexing it
// (the dataroom dashboard included), attributes those trades to the vault, not
// to `operatorAddress`. Without a vault the operator wallet is both.

import type { ConfigBody } from './types'

export interface CapitalLocation {
  /** What holds the capital, in operator words: `vault`, `hot wallet`, `MPC wallet`. */
  label: string
  /** The wallet the chain sees trading. Null when the config has no address yet. */
  address: string | null
  /** Address page for `address`, when it's worth linking. Null otherwise. */
  explorerUrl: string | null
}

/**
 * Read the capital location off a bot's config. Null when there's no readable
 * config — nothing is known, so the caller shows a dash rather than a guess.
 */
export function capitalLocation(
  config: ConfigBody | null | undefined,
): CapitalLocation | null {
  if (!config) return null
  if (config.vaultAddress) {
    return {
      label: 'vault',
      address: config.vaultAddress,
      // A vault is a contract on this chain, so its explorer page is always real.
      explorerUrl: config.vaultExplorerUrl,
    }
  }
  const hot = config.signer === 'hot-wallet'
  return {
    label: hot ? 'hot wallet' : 'MPC wallet',
    address: config.operatorAddress,
    // An MPC signer's address is custodial: no explorer link, same as the
    // Operator row.
    explorerUrl: hot ? config.explorerUrl : null,
  }
}

/**
 * The wallet to ask the dashboard about. The vault when there is one, because
 * that's the address the maker index is keyed by; otherwise the operator wallet.
 */
export function dashboardWallet(
  config: ConfigBody | null | undefined,
): string | null {
  return capitalLocation(config)?.address ?? null
}

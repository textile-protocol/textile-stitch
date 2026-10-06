import { describe, expect, it } from 'vitest'
import { textileVaultUrl } from './capital'
import type { ConfigBody } from './types'

// Only the fields the link reads.
const config = (vaultAddress: string | null, chainId = 42220): ConfigBody =>
  ({ vaultAddress, chainId }) as ConfigBody

describe('textileVaultUrl', () => {
  it("links a vault maker to the vault's page on the Textile app", () => {
    expect(textileVaultUrl(config('0xabc', 8453))).toBe(
      'https://app.textilecredit.com/s/vaults/8453/0xabc',
    )
  })

  it('is null without a vault', () => {
    expect(textileVaultUrl(config(null))).toBeNull()
  })

  it('is null without a readable config', () => {
    expect(textileVaultUrl(null)).toBeNull()
    expect(textileVaultUrl(undefined)).toBeNull()
  })
})

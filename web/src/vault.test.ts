import { describe, expect, it } from 'vitest'
import { ApiError } from './api'
import type { VaultCheck } from './types'
import {
  checksFromError,
  checksVerdict,
  foldRepeatedDetails,
  isVaultAddress,
  sameAddress,
  savedMessage,
  vaultAddressError,
} from './vault'

const VAULT = '0x70997970C51812dc3A010C7d01b50e0d17dc79C8'

const row = (status: VaultCheck['status']): VaultCheck => ({
  id: status,
  label: status,
  status,
  detail: '',
})

describe('isVaultAddress', () => {
  it('takes 0x and 40 hex characters, any case, padded or not', () => {
    expect(isVaultAddress(VAULT)).toBe(true)
    expect(isVaultAddress(VAULT.toLowerCase())).toBe(true)
    expect(isVaultAddress(`  ${VAULT}\n`)).toBe(true)
  })

  it('refuses anything shorter, longer, unprefixed or non-hex', () => {
    expect(isVaultAddress(VAULT.slice(0, 41))).toBe(false)
    expect(isVaultAddress(`${VAULT}0`)).toBe(false)
    expect(isVaultAddress(VAULT.slice(2))).toBe(false)
    expect(isVaultAddress(`0x${'g'.repeat(40)}`)).toBe(false)
  })
})

describe('vaultAddressError', () => {
  it('says nothing while the field is empty or valid', () => {
    expect(vaultAddressError('')).toBeNull()
    expect(vaultAddressError('   ')).toBeNull()
    expect(vaultAddressError(VAULT)).toBeNull()
  })

  it('names what is wrong', () => {
    expect(vaultAddressError(VAULT.slice(2))).toMatch(/starts with 0x/)
    expect(vaultAddressError('0xzz')).toMatch(/0-9 and a-f/)
    expect(vaultAddressError('0x1234')).toMatch(/4 characters .* 40/)
  })
})

describe('sameAddress', () => {
  it('ignores case and padding, and never matches an empty side', () => {
    expect(sameAddress(VAULT, ` ${VAULT.toLowerCase()} `)).toBe(true)
    expect(sameAddress(VAULT, '')).toBe(false)
    expect(sameAddress(null, null)).toBe(false)
  })
})

describe('checksVerdict', () => {
  it('fails on any failure, warns on any warning, else passes', () => {
    expect(checksVerdict([row('ok'), row('warn'), row('fail')])).toBe('fail')
    expect(checksVerdict([row('ok'), row('warn'), row('skipped')])).toBe('warn')
    expect(checksVerdict([row('ok'), row('skipped')])).toBe('pass')
    expect(checksVerdict([])).toBe('pass')
  })
})

describe('checksFromError', () => {
  it('reads the checklist a refused connect sends back', () => {
    const checks = [row('fail')]
    const e = new ApiError(400, 'Nothing was changed.', { error: 'x', checks })
    expect(checksFromError(e)).toEqual(checks)
  })

  it('is null for any other error', () => {
    expect(checksFromError(new ApiError(409, 'Migrate first', { error: 'x' }))).toBeNull()
    expect(checksFromError(new ApiError(0, 'offline'))).toBeNull()
    expect(checksFromError(new Error('boom'))).toBeNull()
  })
})

describe('foldRepeatedDetails', () => {
  it('says why things were skipped once, not on every row', () => {
    const skipped = (id: string): VaultCheck => ({
      id,
      label: id,
      status: 'skipped',
      detail: 'Needs the checks above to pass.',
    })
    const rows = foldRepeatedDetails([
      { id: 'contract', label: 'c', status: 'fail', detail: 'No contract' },
      skipped('views'),
      skipped('pair'),
      { ...skipped('textile'), detail: 'No vault check on this API.' },
    ])
    expect(rows.map((r) => r.showDetail)).toEqual([true, true, false, true])
  })

  it('never shows an empty detail', () => {
    expect(foldRepeatedDetails([{ ...row('ok'), detail: '' }])[0]?.showDetail).toBe(false)
  })
})

describe('savedMessage', () => {
  it('is the message when the restart went through', () => {
    expect(savedMessage({ message: 'Connected.', restartError: null })).toBe('Connected.')
  })

  it('adds a failed restart the message does not mention', () => {
    expect(
      savedMessage({ message: 'Connected.', restartError: 'daemon gone' }),
    ).toBe('Connected. The restart failed: daemon gone. Restart the bot to apply the change.')
  })

  it('does not repeat a failure the message already explains', () => {
    const message = "The change wasn't applied: restarting failed (daemon gone)."
    expect(savedMessage({ message, restartError: 'daemon gone' })).toBe(message)
  })
})

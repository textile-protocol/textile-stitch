// Connecting an OperatorVault: the parts of the Trade from a vault form that
// don't need React. The panel runs the real checks; these only decide what to
// ask it and how to show the answer.

import { ApiError } from './api'
import type { VaultCheck, VaultCheckStatus } from './types'

const ADDRESS = /^0x[0-9a-fA-F]{40}$/

/** Well-formed enough to send for a check: `0x` and 40 hex characters. */
export function isVaultAddress(input: string): boolean {
  return ADDRESS.test(input.trim())
}

/**
 * What's wrong with the typed address, or null when there's nothing to say.
 * Empty is not an error: the operator hasn't typed anything yet. Checksums are
 * the panel's job; this only catches what can't be an address at all.
 */
export function vaultAddressError(input: string): string | null {
  const value = input.trim()
  if (value === '' || isVaultAddress(value)) return null
  if (!value.startsWith('0x')) return 'An address starts with 0x.'
  const hex = value.slice(2)
  if (/[^0-9a-fA-F]/.test(hex)) return 'Only 0-9 and a-f after the 0x.'
  return `That's ${hex.length} characters after the 0x; an address has 40.`
}

/** Same address, whatever the casing. */
export function sameAddress(a: string | null | undefined, b: string | null | undefined): boolean {
  return !!a && !!b && a.trim().toLowerCase() === b.trim().toLowerCase()
}

export type ChecksVerdict = 'pass' | 'warn' | 'fail'

/** One word for a checklist: any failure blocks, a warning doesn't. */
export function checksVerdict(checks: VaultCheck[]): ChecksVerdict {
  if (checks.some((c) => c.status === 'fail')) return 'fail'
  if (checks.some((c) => c.status === 'warn')) return 'warn'
  return 'pass'
}

/** The glyph and the words a screen reader gets for a row's status. */
export const CHECK_MARK: Record<VaultCheckStatus, { icon: string; words: string }> = {
  ok: { icon: '✓', words: 'passed' },
  fail: { icon: '✕', words: 'failed' },
  warn: { icon: '!', words: 'warning' },
  skipped: { icon: '–', words: 'skipped' },
}

/**
 * The checklist a refused connect sent back, if it sent one. The panel puts
 * the rows next to `error` so the form can show which ones stopped it.
 */
export function checksFromError(e: unknown): VaultCheck[] | null {
  if (!(e instanceof ApiError)) return null
  const body = e.body as { checks?: unknown } | null
  return Array.isArray(body?.checks) ? (body.checks as VaultCheck[]) : null
}

/**
 * The rows to render, with the detail of a skipped row hidden when the row
 * before it already said the same thing. A failure early on skips everything
 * after it for one reason, and repeating that reason on every row buries the
 * failure.
 */
export function foldRepeatedDetails(
  checks: VaultCheck[],
): (VaultCheck & { showDetail: boolean })[] {
  return checks.map((check, i) => {
    const previous = i > 0 ? checks[i - 1] : undefined
    const repeat =
      check.status === 'skipped' &&
      previous?.status === 'skipped' &&
      previous.detail === check.detail
    return { ...check, showDetail: check.detail !== '' && !repeat }
  })
}

/**
 * What to tell the operator after a connect or disconnect. The panel's message
 * speaks for the venue side; a restart that failed after the config was saved
 * is reported apart, and it is the part that needs acting on.
 */
export function savedMessage(res: {
  message: string
  restartError: string | null
}): string {
  const failed = res.restartError?.trim()
  if (!failed || res.message.includes(failed)) return res.message
  return `${res.message} The restart failed: ${failed}. Restart the bot to apply the change.`
}

import type { PerpsView } from './api'

export function hedgeMode(view: PerpsView | null) {
  if (!view) return 'unknown'
  if (!view.enabled) return 'off'
  const mode = view.remote?.control?.mode
  return mode === 'live' || mode === 'preview' || mode === 'off'
    ? mode
    : 'unknown'
}

export function hedgeSnapshotCurrent(view: PerpsView, now: number) {
  const at =
    view.remote?.monitoring?.observedAt ??
    view.remote?.recall?.position?.observedAt
  const timestamp = at ? Date.parse(at) : NaN
  return timestamp <= now && now - timestamp <= 15_000
}

export function hedgeStatus(
  view: PerpsView | null,
  now: number,
  failed = false
) {
  if (failed || view?.unavailable) return 'Unavailable'
  if (view?.remote?.recall?.position?.pauseReason) return 'Paused'
  const mode = hedgeMode(view)
  if (mode === 'unknown') return 'Loading'
  if (mode === 'off') return 'Off'
  if (mode === 'preview') return 'Preview'
  if (view?.remote?.recall?.executionEnabled === false || !view?.recallEnabled)
    return 'Execution disabled'
  if (view.remote?.effectiveMode !== 'live') return 'Awaiting authorization'
  if (!hedgeSnapshotCurrent(view, now)) return 'Waiting for snapshot'
  return 'Live'
}

/** An unavailable or old snapshot cannot prove a disabled hedge is empty.
 * Ordinary wallet inventory alone does not keep an otherwise empty hedge open. */
export function showHedgeModule(view: PerpsView | null, now: number) {
  if (!view) return false
  if (view.enabled) return true
  if (!view.assignmentId) return false
  const monitoring = view.remote?.monitoring
  if (view.unavailable || !monitoring || !hedgeSnapshotCurrent(view, now))
    return true
  return (
    monitoring.hasFunds ||
    monitoring.pendingQuotes > 0 ||
    monitoring.pendingActions > 0
  )
}

export function allocationAtoms(
  value: string,
  decimals: number
): string | null {
  if (!Number.isInteger(decimals) || decimals < 0 || decimals > 18) return null
  const match = /^(\d+)(?:\.(\d+))?$/.exec(value.trim())
  if (!match || (match[2]?.length ?? 0) > decimals) return null
  return (
    BigInt(match[1]!) * 10n ** BigInt(decimals) +
    BigInt((match[2] ?? '').padEnd(decimals, '0') || '0')
  ).toString()
}

export function allocationTokens(atoms: string, decimals: number) {
  const value = BigInt(atoms || '0')
  if (decimals === 0) return value.toString()
  const scale = 10n ** BigInt(decimals)
  return `${value / scale}.${(value % scale).toString().padStart(decimals, '0')}`
}

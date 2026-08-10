// Formatting helpers.
//
// Everything here goes through `Intl` with no explicit locale, so the operator's
// own locale decides separators and date order. Amounts are atomic-unit integers
// arriving as strings; they are formatted with BigInt arithmetic rather than
// `Number`, because a large inventory would lose its low digits to a float.

/** The literal the config accepts for "quote everything funded". */
export const MAX_LIQUIDITY = 'max'

/**
 * Render an atomic-unit integer string as a decimal amount.
 *
 * Returns the input unchanged for the `max` sentinel and for anything that isn't
 * a plain integer, so a hand-edited config renders as what it actually says
 * instead of as `NaN`.
 */
export function formatAtomic(raw: string, decimals: number): string {
  const value = raw.trim()
  if (value === '' || value === MAX_LIQUIDITY) return value
  if (!/^\d+$/.test(value)) return value

  const big = BigInt(value)
  const scale = 10n ** BigInt(decimals)
  const whole = big / scale
  const frac = big % scale

  const wholeText = new Intl.NumberFormat().format(whole)
  if (frac === 0n) return wholeText

  // Keep at most 6 fractional digits, trimmed: a wei-exact tail is noise in a UI.
  const fracText = frac.toString().padStart(decimals, '0').slice(0, 6).replace(/0+$/, '')
  if (fracText === '') return wholeText
  const separator = decimalSeparator()
  return `${wholeText}${separator}${fracText}`
}

function decimalSeparator(): string {
  return (
    new Intl.NumberFormat()
      .formatToParts(1.1)
      .find((p) => p.type === 'decimal')?.value ?? '.'
  )
}

/** A unix timestamp as a locale date-time, or a dash when there isn't one. */
export function formatTimestamp(unix: number | null | undefined): string {
  if (!unix) return '—'
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: 'medium',
    timeStyle: 'short',
  }).format(new Date(unix * 1000))
}

/**
 * An RFC 3339 timestamp as a locale date, or a dash.
 *
 * Separate from [`formatTimestamp`] because the source is different: this one
 * comes from a registry / commit record as a string, and an unparseable value
 * has to read as "unknown" rather than as `Invalid Date`.
 */
export function formatDate(iso: string | null | undefined): string {
  if (!iso) return '—'
  const at = new Date(iso)
  if (Number.isNaN(at.getTime())) return '—'
  return new Intl.DateTimeFormat(undefined, { dateStyle: 'medium' }).format(at)
}

/** A duration in seconds, as the shortest sensible unit. */
export function formatSeconds(secs: number): string {
  if (secs < 60) return `${secs}s`
  if (secs < 3600) return `${Math.round(secs / 60)}m`
  return `${Math.round(secs / 360) / 10}h`
}

/** `0x1234…abcd`, for addresses that don't need to be read in full. */
export function shortAddress(address: string): string {
  return address.length > 12
    ? `${address.slice(0, 6)}…${address.slice(-4)}`
    : address
}

/**
 * An image reference short enough for a fleet row or detail cell.
 *
 * Drops the registry path, then centre-truncates a content digest so a bare
 * `sha256:…` (or `name@sha256:…`) does not shove the rest of the layout off
 * screen. Tagged refs like `textile-stitch:latest` stay as-is.
 */
export function shortImage(image: string | null): string {
  if (!image) return '—'
  const lastSlash = image.lastIndexOf('/')
  const name = lastSlash === -1 ? image : image.slice(lastSlash + 1)
  return shortenDigest(name)
}

/** `sha256:ce1d74…2fef60` — keeps the prefix and enough hex to tell digests apart. */
function shortenDigest(ref: string): string {
  const at = ref.lastIndexOf('@')
  if (at !== -1) {
    const name = ref.slice(0, at)
    const digest = ref.slice(at + 1)
    return `${name}@${shortenSha256(digest)}`
  }
  return shortenSha256(ref)
}

function shortenSha256(value: string): string {
  const hex = value.startsWith('sha256:') ? value.slice('sha256:'.length) : null
  if (hex === null || hex.length <= 12) return value
  return `sha256:${hex.slice(0, 6)}…${hex.slice(-6)}`
}

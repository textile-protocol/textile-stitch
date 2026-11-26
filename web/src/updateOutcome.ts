// What the page that loads after a Fleet UI update says about it.
//
// Updating Fleet UI restarts the panel and reloads this page, so the page that
// started the update can't show how it ended. Just before the restart it
// leaves a note in the tab's sessionStorage: the version it moved from and how
// many bots it updated first. The page that loads afterwards reads the note
// once and shows the outcome.
//
// The note is for display only. Nothing acts on it: the bots were already
// updated, on the old page, before the restart. Past OUTCOME_TTL_MS it's
// dropped unread, so a tab restored much later doesn't announce an old update.

export const OUTCOME_KEY = 'stitch-fleet-update-outcome'

/** A restart takes seconds and a sign-in a minute or two. */
export const OUTCOME_TTL_MS = 15 * 60 * 1000

const MAX_VERSION = 64
const SKEW_MS = 60 * 1000

export interface UpdateOutcome {
  /** The Fleet UI version the update started from. */
  from: string
  /** Bots updated before the restart. */
  bots: number
  /** Bots that didn't come back up, when the operator updated Fleet UI anyway. */
  failed: number
  /** When the note was written, in ms since the epoch. */
  at: number
}

export function encodeOutcome(o: UpdateOutcome): string {
  return JSON.stringify({ v: 1, from: o.from, bots: o.bots, failed: o.failed, at: o.at })
}

/** The note, or null when there isn't a current one in exactly this shape. */
export function decodeOutcome(raw: string | null, now: number): UpdateOutcome | null {
  if (!raw) return null
  let value: unknown
  try {
    value = JSON.parse(raw)
  } catch {
    return null
  }
  if (typeof value !== 'object' || value === null) return null
  const v = value as Record<string, unknown>
  if (v.v !== 1) return null
  if (typeof v.from !== 'string' || v.from.length > MAX_VERSION) return null
  if (!isCount(v.bots) || !isCount(v.failed)) return null
  if (typeof v.at !== 'number' || !Number.isFinite(v.at)) return null
  if (v.at > now + SKEW_MS || now - v.at > OUTCOME_TTL_MS) return null
  return { from: v.from, bots: v.bots, failed: v.failed, at: v.at }
}

function isCount(n: unknown): n is number {
  return typeof n === 'number' && Number.isInteger(n) && n >= 0
}

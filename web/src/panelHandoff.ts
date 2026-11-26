// The bots Update panel takes with it, carried across the panel's restart.
//
// The panel updates first and the bots after. Bot containers are recreated by
// the panel, so the release that ships the new bot image should be the one
// that recreates them. But a panel update restarts the panel and reloads this
// page, and bot updates run from the page. So once the panel update is armed,
// the bots the operator ticked go into sessionStorage, and the page that
// loads on the new panel reads them back and starts the run.
//
// What that note can and can't do:
// - It only narrows. The new page plans again from what the new panel reports
//   and updates a bot only if it's on the note, still behind and updatable.
//   A note can't add a bot the operator didn't tick, or one the planner skips
//   (pinned, flat layout, not editable).
// - It belongs to one tab. sessionStorage is per tab, so a second open tab
//   never picks it up and runs the same updates twice. Closing the tab drops it.
// - It's read once. The page removes it before acting on it, so reloading
//   mid-run doesn't start the run over.
// - It goes stale. Past HANDOFF_TTL_MS it's dropped unread, so a tab restored
//   hours later doesn't start recreating bots by itself.
// - It runs only if the panel moved. A panel that still reports itself behind
//   didn't land on the new image, and its bots stay where they are.

import { planFleetUpdate, type UpdateCandidate } from './fleetUpdate'
import type { Bot, UpdatesStatus } from './types'

export const HANDOFF_KEY = 'stitch-panel-bots-after-update'

/**
 * How long a note stays good. A panel restart takes seconds and a sign-in a
 * minute or two; this leaves room for a slow pull or a slow operator without
 * letting a forgotten tab act on a choice made long ago.
 */
export const HANDOFF_TTL_MS = 30 * 60 * 1000

/** Fleets are tens of bots; anything past this isn't a note this page wrote. */
const MAX_BOTS = 500
const MAX_NAME = 128
/** Allowed clock skew for a note dated slightly in the future. */
const SKEW_MS = 60 * 1000

export interface BotsAfterPanel {
  /** Bot names the operator ticked, in the order they'll update. */
  bots: string[]
  /** The panel version that wrote the note, for the "from → to" line. */
  from: string
  /** When the note was written, in ms since the epoch. */
  at: number
}

export type HandoffRead =
  | { kind: 'none' }
  | { kind: 'expired'; handoff: BotsAfterPanel }
  | { kind: 'ready'; handoff: BotsAfterPanel }

/** The stored form. `v` lets a later panel tell an older note apart. */
export function encodeHandoff(h: BotsAfterPanel): string {
  return JSON.stringify({ v: 1, bots: h.bots, from: h.from, at: h.at })
}

/**
 * Read a stored note. Anything that isn't exactly the shape this page writes
 * (another version, a hand-edited value, a note dated in the future) reads as
 * no note at all.
 */
export function decodeHandoff(raw: string | null, now: number): HandoffRead {
  const handoff = parse(raw)
  if (!handoff || handoff.at > now + SKEW_MS) return { kind: 'none' }
  if (now - handoff.at > HANDOFF_TTL_MS) return { kind: 'expired', handoff }
  return { kind: 'ready', handoff }
}

function parse(raw: string | null): BotsAfterPanel | null {
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
  if (typeof v.from !== 'string' || v.from.length > MAX_NAME) return null
  if (typeof v.at !== 'number' || !Number.isFinite(v.at)) return null
  if (!Array.isArray(v.bots) || v.bots.length === 0 || v.bots.length > MAX_BOTS) return null
  const names = v.bots.filter(
    (b): b is string => typeof b === 'string' && b.length > 0 && b.length <= MAX_NAME,
  )
  if (names.length !== v.bots.length) return null
  return { bots: [...new Set(names)], from: v.from, at: v.at }
}

/** Why the bots on a note didn't update. */
export type AfterPanelSkip =
  /** The panel still reports itself behind: the swap didn't land. */
  | 'panel-not-moved'
  /** The registry check for the bot image failed, so nothing reads as behind. */
  | 'registry'
  /** Every ticked bot is current, gone, or one the planner leaves alone. */
  | 'nothing-behind'
  /** The note was older than HANDOFF_TTL_MS. */
  | 'expired'
  /** Reading the fleet or the update check failed. */
  | 'check-failed'

export type AfterPanelPlan =
  | { kind: 'run'; target: string; picked: UpdateCandidate[] }
  | { kind: 'skip'; reason: AfterPanelSkip }

/**
 * What to do with a note on the new panel. The run is the note's bots, in the
 * note's order, kept only where the new panel's own plan says they're ready.
 */
export function planAfterPanel(
  handoff: BotsAfterPanel,
  bots: Bot[],
  updates: UpdatesStatus,
): AfterPanelPlan {
  if (updates.panel.updateAvailable) return { kind: 'skip', reason: 'panel-not-moved' }
  const ready = new Map(planFleetUpdate(bots, updates).ready.map((c) => [c.name, c]))
  const picked = handoff.bots
    .map((name) => ready.get(name))
    .filter((c): c is UpdateCandidate => c !== undefined)
  if (picked.length > 0) return { kind: 'run', target: updates.bot.targetImage, picked }
  return { kind: 'skip', reason: updates.bot.reason ? 'registry' : 'nothing-behind' }
}

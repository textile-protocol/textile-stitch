// Update every bot that's behind, one at a time.
//
// The panel already knows how to update one bot (POST /api/bots/{name}/update),
// with every guard that matters: strict pull, flat-layout refusal, channel check,
// wallet claim. Updating the fleet is that call in a loop, run by the page, so
// nothing about what a single update is allowed to do changes here.
//
// One at a time on purpose. A release that crash-loops on startup should cost
// one bot's quoting, not the whole fleet's, so the run stops at the first bot
// that doesn't come back up and leaves the rest where they were.

import { botLabel } from './botRoutes'
import type { Bot, BotState, UpdatesStatus } from './types'

/** A bot the run can update. */
export interface UpdateCandidate {
  name: string
  label: string
  image: string | null
  version: string | null
  /** Up before the update, so it's restarted after and must come back. */
  wantsUp: boolean
}

/** A bot that's behind but can't be updated from here, and why. */
export interface UpdateSkip {
  name: string
  label: string
  reason: string
}

export interface FleetUpdatePlan {
  /** Ticked by default. */
  ready: UpdateCandidate[]
  /** Behind, but left as they are: pinned on purpose, or the update would be refused. */
  skipped: UpdateSkip[]
}

/** Same rule as the server's `is_pin_image_ref`: sha-* tags, digests, bare ids. */
export function isPinnedImage(image: string | null): boolean {
  if (!image) return false
  if (image.includes('@') || image.startsWith('sha256:')) return true
  const lastSlash = image.lastIndexOf('/')
  const tag = image.slice(lastSlash + 1).split(':')[1] ?? ''
  return tag.startsWith('sha-')
}

/** Docker states where the operator means the bot to be up. Mirrors `wants_to_be_up`. */
export function wantsToBeUp(state: BotState): boolean {
  return state === 'running' || state === 'restarting' || state === 'paused'
}

/**
 * Sort the fleet into what Update all would do, in fleet order. Only bots the
 * registry check says are behind are listed: a bot that's current has nothing
 * to update, and one off the panel's channel never shows as behind.
 */
export function planFleetUpdate(bots: Bot[], updates: UpdatesStatus | null): FleetUpdatePlan {
  const behind = new Set(
    (updates?.bots ?? []).filter((b) => b.updateAvailable && b.canUpdate).map((b) => b.name),
  )
  const listed = bots.filter((b) => behind.has(b.name))
  const skips = listed.map((b) => ({ bot: b, reason: skipReason(b) }))
  return {
    ready: skips.filter((s) => s.reason === null).map((s) => candidate(s.bot)),
    skipped: skips
      .filter((s): s is { bot: Bot; reason: string } => s.reason !== null)
      .map((s) => ({ name: s.bot.name, label: botLabel(s.bot), reason: s.reason })),
  }
}

/**
 * Why Update all leaves a behind bot alone. A pin is usually a rollback, and a
 * fleet-wide action shouldn't undo one; the bot's own Update button still can.
 * The rest are the refusals the single-bot endpoint makes, said up front.
 */
function skipReason(bot: Bot): string | null {
  if (isPinnedImage(bot.image)) {
    return 'Pinned to a build, usually after a rollback. Use Update on its page to leave the pin.'
  }
  if (bot.canMigrate || bot.layout === 'flat-files') {
    return 'Still on the flat layout. Migrate it first so the nonce ledger survives.'
  }
  if (!bot.container) return 'Has no container. Recreate it from its page.'
  const blocking = bot.warnings.find((w) => w.blocksEditing)
  if (!bot.editable) return blocking?.message ?? "The panel can't edit this bot."
  return null
}

function candidate(bot: Bot): UpdateCandidate {
  return {
    name: bot.name,
    label: botLabel(bot),
    image: bot.image,
    version: bot.version,
    wantsUp: wantsToBeUp(bot.state),
  }
}

/** The bots a fresh sheet starts with ticked: everything it can update. */
export function defaultSelection(plan: FleetUpdatePlan): string[] {
  return plan.ready.map((c) => c.name)
}

/** One bot's progress through the run. */
export type StepStatus =
  | 'queued'
  /** Pulling the image and recreating the container. */
  | 'updating'
  /** Recreated; waiting for the container to stay up. */
  | 'checking'
  | 'done'
  | 'failed'
  /** Never started: the run was stopped first. */
  | 'untouched'

export interface UpdateStep {
  name: string
  label: string
  wantsUp: boolean
  status: StepStatus
  /** What happened, in the server's words when it answered. */
  detail: string | null
}

export type RunPhase = 'running' | 'stopping' | 'done' | 'cancelled'

export interface FleetUpdateRun {
  target: string
  phase: RunPhase
  steps: UpdateStep[]
}

export function startRun(target: string, picked: UpdateCandidate[]): FleetUpdateRun {
  return {
    target,
    phase: 'running',
    steps: picked.map((c) => ({
      name: c.name,
      label: c.label,
      wantsUp: c.wantsUp,
      status: 'queued',
      detail: null,
    })),
  }
}

export function setStep(
  run: FleetUpdateRun,
  name: string,
  status: StepStatus,
  detail: string | null = null,
): FleetUpdateRun {
  return {
    ...run,
    steps: run.steps.map((s) => (s.name === name ? { ...s, status, detail } : s)),
  }
}

/** End the run: anything still queued is marked untouched. */
export function finishRun(run: FleetUpdateRun, phase: RunPhase): FleetUpdateRun {
  return {
    ...run,
    phase,
    steps: run.steps.map((s) => (s.status === 'queued' ? { ...s, status: 'untouched' } : s)),
  }
}

export function nextQueued(run: FleetUpdateRun): UpdateStep | undefined {
  return run.steps.find((s) => s.status === 'queued')
}

export function runCounts(run: FleetUpdateRun): {
  done: number
  failed: number
  /** Finished either way, for the progress bar. */
  settled: number
  total: number
} {
  const count = (status: StepStatus) => run.steps.filter((s) => s.status === status).length
  const done = count('done')
  const failed = count('failed')
  return { done, failed, settled: done + failed, total: run.steps.length }
}

export function runActive(run: FleetUpdateRun | null): boolean {
  return run?.phase === 'running' || run?.phase === 'stopping'
}

/**
 * Reads in a row a recreated bot has to be `running` before the run moves on.
 * At the runner's 2s poll that's about ten seconds up: a release that dies on
 * startup is usually back in `restarting` well inside that, and one read can
 * land in the gap between a crash and Docker's restart.
 */
export const STABLE_READS = 5

export type HealthVerdict = 'up' | 'down' | 'waiting'

/**
 * Whether a recreated bot came back, from the states read since, oldest first.
 * A bot that was stopped before is left stopped, so there's nothing to wait for.
 * `exited` or `dead` is down at once: the restart policy has given up.
 */
export function healthVerdict(
  wantsUp: boolean,
  seen: BotState[],
  stableReads = STABLE_READS,
): HealthVerdict {
  if (!wantsUp) return 'up'
  const last = seen[seen.length - 1]
  if (last === 'exited' || last === 'dead') return 'down'
  const tail = seen.slice(-stableReads)
  if (tail.length === stableReads && tail.every((s) => s === 'running')) return 'up'
  return 'waiting'
}

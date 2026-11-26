// What the header strip says while Stitch Fleet updates: the bots first, then
// Fleet UI (the panel itself), then what happened, on the page that loads
// after the restart.
//
// Operators see the panel as "Fleet UI", and most of them think in trades and
// balances, not containers. So the copy never says panel, image, container or
// registry, and it says what happens to their bots.
//
// The order is bots first on purpose. The bots update one at a time on the
// page the operator is signed in to, where they can watch, stop the run, or
// roll back a bot that didn't come back. Fleet UI restarts last, when no bot
// is mid-update, and only if every bot came back: a restart signs a password
// session out, and nobody should have to sign back in to fix a bot that's down.
// A Fleet UI restart doesn't touch the bots, which keep trading through it.
//
// Three stores feed the strip, in this order of precedence:
// - `PanelUpdate`: the Fleet UI update in flight (download, restart, reconnect).
// - `FleetUpgrade`: the bot updates that come before it.
// - `FleetOutcome`: on the page after the restart, how it ended.
// `AfterPanel` is the older flow, Fleet UI first and bots after the reload,
// which pages from v0.1.399 and v0.1.400 still start. See panelHandoff.ts.

import { type FleetUpdateRun, runActive, runCounts } from './fleetUpdate'
import { HANDOFF_TTL_MS, type AfterPanelSkip } from './panelHandoff'

/** Shared by every step of a Fleet UI update. */
interface FleetUiStep {
  /** Bots updated before it in this flow; 0 for Fleet UI alone. */
  bots: number
  /** A password session, which the restart signs out. */
  signsOut: boolean
}

export type PanelUpdate =
  | { phase: 'idle' }
  /** POST /api/panel/update is in flight: the new version is downloading. */
  | ({ phase: 'pulling' } & FleetUiStep)
  /** The restart is scheduled; the old Fleet UI still answers. */
  | ({ phase: 'armed' } & FleetUiStep)
  /** Fleet UI stopped answering: it's restarting. */
  | ({ phase: 'restarting' } & FleetUiStep)
  /** Fleet UI is back (or signed us out); the page is reloading onto it. */
  | ({ phase: 'reloading' } & FleetUiStep)
  /** Polling gave up before Fleet UI came back. */
  | ({ phase: 'slow' } & FleetUiStep)
  /** The update request itself was refused or errored. */
  | ({ phase: 'failed'; error: string } & FleetUiStep)

/** The bot updates that run before Fleet UI, on the same page. */
export type FleetUpgrade =
  | { phase: 'idle' }
  /** Bots are updating; Fleet UI updates after the last one. */
  | { phase: 'bots'; from: string; signsOut: boolean }
  /** Not every bot came back, or the operator stopped the run, so Fleet UI waits. */
  | {
      phase: 'held'
      from: string
      signsOut: boolean
      done: number
      failed: number
      total: number
      stopped: boolean
    }

/** On the page that loads after Fleet UI restarted. */
export type FleetOutcome =
  | { phase: 'none' }
  | { phase: 'updated'; from: string; version: string; bots: number; failed: number }
  /** It came back still reporting an update: the new version didn't land. */
  | { phase: 'not-moved'; bots: number }

/** Older flow: on the new Fleet UI, the bots an older page handed over. */
export type AfterPanel =
  | { phase: 'none' }
  /** Reading the fleet and the registry before deciding which bots run. */
  | { phase: 'checking'; from: string; version: string; bots: number }
  /** The bot run started. Its progress is the run's own. */
  | { phase: 'bots'; from: string; version: string }
  /** The bots didn't run, and why. */
  | {
      phase: 'skipped'
      from: string
      version: string
      reason: AfterPanelSkip
      error?: string
    }

export const FLEET_UI_STEPS = ['Download', 'Restart', 'Reconnect'] as const
const BOTS_FIRST_STEPS = ['Update bots', ...FLEET_UI_STEPS] as const
/** What a v0.1.399–v0.1.400 page showed before the reload: Fleet UI, then bots. */
const LEGACY_STEPS = [...FLEET_UI_STEPS, 'Update bots'] as const
const LEGACY_BOTS_STEP = LEGACY_STEPS.length - 1

/** The steps of a flow that updates `bots` bots before Fleet UI. */
export function fleetUpdateSteps(bots: number): readonly string[] {
  return bots > 0 ? BOTS_FIRST_STEPS : FLEET_UI_STEPS
}

export type StripAction =
  | 'reload'
  /** A link to the Fleet page; hidden when already on it. */
  | 'fleet'
  /** Go on and update Fleet UI after the bots were held. */
  | 'fleet-ui'

export interface PanelUpdateView {
  tone: 'progress' | 'success' | 'warning' | 'danger'
  title: string
  detail: string
  steps: readonly string[]
  /**
   * Index into `steps` of the step under way. `steps.length` ticks them all;
   * null hides them.
   */
  step: number | null
  actions: StripAction[]
  dismissible: boolean
}

export function panelUpdateView(u: PanelUpdate): PanelUpdateView | null {
  if (u.phase === 'idle') return null
  // With bots first, "Update bots" is step 0 and already done.
  const offset = u.bots > 0 ? 1 : 0
  const progress = (step: number, detail: string): PanelUpdateView => ({
    ...base(fleetUpdateSteps(u.bots)),
    tone: 'progress',
    title: u.bots > 0 ? 'Bots updated · updating Fleet UI' : 'Updating Fleet UI',
    detail,
    step: step + offset,
  })
  switch (u.phase) {
    case 'pulling':
      return progress(0, 'Downloading the update. Your bots keep trading.')
    case 'armed':
      return progress(
        1,
        u.signsOut
          ? "Fleet UI restarts in a moment. Your bots keep trading. You'll sign in again once it's back."
          : 'Fleet UI restarts in a moment. Your bots keep trading.',
      )
    case 'restarting':
      return progress(
        1,
        u.signsOut
          ? 'Your bots keep trading. This page comes back in a few seconds and asks you to sign in again.'
          : 'Your bots keep trading. This page comes back by itself in a few seconds.',
      )
    case 'reloading':
      return progress(2, 'Fleet UI is back. Opening the new version.')
    case 'slow':
      return {
        ...base(fleetUpdateSteps(u.bots)),
        tone: 'warning',
        title: 'Fleet UI is taking a while',
        detail: "It isn't back yet. Your bots keep trading. Reload in a moment.",
        actions: ['reload'],
      }
    case 'failed':
      return {
        ...base(fleetUpdateSteps(u.bots)),
        tone: 'danger',
        title: "Fleet UI update didn't start",
        detail: u.bots > 0 ? `${u.error} Your bots are updated and trading.` : u.error,
        dismissible: true,
      }
  }
}

/**
 * The bot updates before Fleet UI. `onFleet` is true on the Fleet page, where
 * the run's own card lists each bot.
 */
export function upgradeView(
  g: FleetUpgrade,
  run: FleetUpdateRun | null,
  onFleet = false,
): PanelUpdateView | null {
  if (g.phase === 'idle') return null
  if (g.phase === 'held') return heldView(g, onFleet)
  if (!run) return null
  const c = runCounts(run)
  return {
    ...base(BOTS_FIRST_STEPS),
    tone: 'progress',
    title: `Updating bots · ${c.settled} of ${c.total}${c.failed > 0 ? ` · ${c.failed} failed` : ''}`,
    detail:
      run.phase === 'stopping'
        ? "Finishing the current bot, then stopping. Fleet UI won't update."
        : onFleet
          ? "Each bot's progress is below. Fleet UI updates after the last one. Keep this tab open."
          : 'One at a time. Fleet UI updates after the last one. Keep this tab open.',
    step: 0,
    actions: ['fleet'],
  }
}

function heldView(g: Extract<FleetUpgrade, { phase: 'held' }>, onFleet: boolean): PanelUpdateView {
  const held = {
    ...base(BOTS_FIRST_STEPS),
    actions: ['fleet-ui', 'fleet'] as StripAction[],
    dismissible: true,
  }
  if (g.failed > 0) {
    const one = g.failed === 1
    return {
      ...held,
      tone: 'danger',
      title: `Fleet UI on hold · ${g.failed} of ${botCount(g.total)} didn't come back`,
      detail:
        `${g.done} updated fine. Fleet UI waits so you can deal with ${one ? 'it' : 'them'} first: ` +
        (onFleet
          ? `open ${one ? 'the one' : 'the ones'} that failed, below, to see what happened or go back to the previous version.`
          : `view your bots to see which ${one ? 'one' : 'ones'}.`),
    }
  }
  return {
    ...held,
    tone: 'warning',
    title: 'Fleet UI on hold · bot updates stopped',
    detail: `${g.done} of ${botCount(g.total)} updated before you stopped. The rest stay on their current version.`,
  }
}

/** On the page after the restart. */
export function outcomeView(o: FleetOutcome): PanelUpdateView | null {
  switch (o.phase) {
    case 'none':
      return null
    case 'updated': {
      const steps = fleetUpdateSteps(o.bots)
      const one = o.failed === 1
      return {
        ...base(steps),
        // Updated anyway after a hold: a bot is still down, so not all green.
        tone: o.failed > 0 ? 'warning' : 'success',
        title: o.bots > 0 ? `Fleet UI and ${botCount(o.bots)} updated` : 'Fleet UI updated',
        detail:
          fleetNow(o.from, o.version) +
          (o.failed > 0
            ? ` ${botCount(o.failed)} didn't come back up after ${one ? 'its' : 'their'} update. Open ${one ? 'it' : 'them'} from your bots to see what happened.`
            : o.bots > 0
              ? ' Your bots updated first and all came back up.'
              : ''),
        step: steps.length,
        actions: o.failed > 0 ? ['fleet'] : [],
        dismissible: true,
      }
    }
    case 'not-moved':
      return {
        ...base(fleetUpdateSteps(o.bots)),
        tone: 'warning',
        title: "Fleet UI didn't update",
        detail:
          'It came back on the same version. Try Update again in a few minutes.' +
          (o.bots > 0 ? ' Your bots were updated and are trading.' : ''),
        dismissible: true,
      }
  }
}

/**
 * The older flow's strip on the new Fleet UI, from the handover and the bot
 * run it started.
 */
export function afterPanelView(
  a: AfterPanel,
  run: FleetUpdateRun | null,
  onFleet = false,
): PanelUpdateView | null {
  switch (a.phase) {
    case 'none':
      return null
    case 'checking':
      return {
        ...base(LEGACY_STEPS),
        tone: 'progress',
        title: 'Fleet UI updated',
        detail: `${fleetNow(a.from, a.version)} Checking which bots still need the update.`,
        step: LEGACY_BOTS_STEP,
      }
    case 'bots':
      return run ? legacyBotsView(a, run, onFleet) : null
    case 'skipped':
      return skippedView(a)
  }
}

function legacyBotsView(
  a: { from: string; version: string },
  run: FleetUpdateRun,
  onFleet: boolean,
): PanelUpdateView {
  const c = runCounts(run)
  const now = fleetNow(a.from, a.version)
  if (runActive(run)) {
    return {
      ...base(LEGACY_STEPS),
      tone: 'progress',
      title: `Fleet UI updated · bots ${c.settled} of ${c.total}${c.failed > 0 ? ` · ${c.failed} failed` : ''}`,
      detail:
        run.phase === 'stopping'
          ? 'Finishing the current bot, then stopping.'
          : `${now} Your bots update one at a time${onFleet ? ', below' : ''}. Keep this tab open until they're done.`,
      step: LEGACY_BOTS_STEP,
      actions: ['fleet'],
    }
  }
  if (c.failed > 0) {
    return {
      ...base(LEGACY_STEPS),
      tone: 'danger',
      title: `Fleet UI updated · ${c.failed} of ${botCount(c.total)} didn't come back`,
      detail: failedDetail(c.done, c.failed, onFleet),
      actions: ['fleet'],
      dismissible: true,
    }
  }
  if (run.phase === 'cancelled') {
    return {
      ...base(LEGACY_STEPS),
      tone: 'warning',
      title: 'Fleet UI updated · bot updates stopped',
      detail: `${c.done} of ${botCount(c.total)} updated before you stopped. The rest stay on their current version.`,
      actions: ['fleet'],
      dismissible: true,
    }
  }
  return {
    ...base(LEGACY_STEPS),
    tone: 'success',
    title: `Fleet UI and ${botCount(c.total)} updated`,
    detail: `${now} Every bot is back up on the latest version.`,
    step: LEGACY_STEPS.length,
    dismissible: true,
  }
}

function failedDetail(done: number, failed: number, onFleet: boolean): string {
  const fine = `${done} updated fine.`
  if (!onFleet) return `${fine} View your bots to see which ${failed === 1 ? 'one' : 'ones'}.`
  return failed === 1
    ? `${fine} Open the one that failed, below, to see what happened or go back to its previous version.`
    : `${fine} Open the ones that failed, below, to see what happened or go back to their previous version.`
}

function skippedView(a: Extract<AfterPanel, { phase: 'skipped' }>): PanelUpdateView {
  const now = fleetNow(a.from, a.version)
  const done = { ...base(FLEET_UI_STEPS), dismissible: true }
  switch (a.reason) {
    case 'panel-not-moved':
      return {
        ...done,
        tone: 'warning',
        title: "Fleet UI didn't update",
        detail:
          "It came back on the same version, so your bots weren't touched. Try Update again in a few minutes.",
      }
    case 'registry':
      return {
        ...done,
        tone: 'warning',
        title: 'Fleet UI updated, bots not checked',
        detail: `${now} We couldn't check for new bot versions, so no bots were updated. Try Update all in a few minutes.`,
        actions: ['fleet'],
      }
    case 'check-failed':
      return {
        ...done,
        tone: 'warning',
        title: 'Fleet UI updated, bots not checked',
        detail: `${now} We couldn't load your bots, so none were updated. Try Update all in a few minutes.`,
        actions: ['fleet'],
      }
    case 'expired':
      return {
        ...done,
        tone: 'warning',
        title: 'Bot updates not started',
        detail: `They were queued more than ${Math.round(HANDOFF_TTL_MS / 60_000)} minutes ago, so they didn't start. If your bots still need it, use Update all.`,
        actions: ['fleet'],
      }
    case 'nothing-behind':
      return {
        ...done,
        tone: 'success',
        title: 'Fleet UI updated',
        detail: `${now} Your bots were already up to date.`,
      }
  }
}

/** True while a Fleet UI update is under way and the header button should stay busy. */
export function panelUpdateBusy(u: PanelUpdate): boolean {
  return u.phase !== 'idle' && u.phase !== 'failed' && u.phase !== 'slow'
}

/**
 * True while bots are updating ahead of Fleet UI, Fleet UI is restarting, or
 * the older handover is deciding which bots to run. Update all stays out of
 * the way then: the flow is about to start, or is running, a run of its own.
 */
export function panelFlowBusy(u: PanelUpdate, a: AfterPanel, g: FleetUpgrade): boolean {
  return panelUpdateBusy(u) || a.phase === 'checking' || g.phase === 'bots'
}

function base(steps: readonly string[]): Omit<PanelUpdateView, 'tone' | 'title' | 'detail'> {
  return { steps, step: null, actions: [], dismissible: false }
}

function botCount(n: number): string {
  return `${n} ${n === 1 ? 'bot' : 'bots'}`
}

/** "Fleet UI is now on v0.1.240 (was v0.1.233).", or without the "was" when it held. */
export function fleetNow(from: string, to: string): string {
  return from && from !== to
    ? `Fleet UI is now on v${to} (was v${from}).`
    : `Fleet UI is now on v${to}.`
}

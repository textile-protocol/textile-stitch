// What the header strip says while the panel updates itself, and the bots
// after it when the operator ticked them.
//
// Operators see the panel as "Fleet" (Stitch Fleet), and most of them think
// in trades and balances, not containers. So the copy never says panel,
// image, container or registry, and it says what happens to their bots.
//
// The panel update is three things the operator waits through: the panel
// pulls the new image, the container is swapped (this page loses its
// connection), and the page reloads onto the new build. With bots ticked
// there's a fourth: the page that loads on the new panel updates them, one at
// a time. The first three happen on the old page (`PanelUpdate`), the fourth
// on the new one (`AfterPanel`), and both show the same four steps so the
// strip reads as one flow across the reload.

import { type FleetUpdateRun, runActive, runCounts } from './fleetUpdate'
import { HANDOFF_TTL_MS, type AfterPanelSkip } from './panelHandoff'

export type PanelUpdate =
  | { phase: 'idle' }
  /** POST /api/panel/update is in flight: the panel is pulling the image. */
  | { phase: 'pulling'; bots: number }
  /** Swap is scheduled; the old panel still answers. */
  | { phase: 'armed'; bots: number }
  /** A session poll failed, so the container is being recreated. */
  | { phase: 'restarting'; bots: number }
  /** The panel answered again; the page is reloading onto the new UI. */
  | { phase: 'reloading'; bots: number }
  /** Polling gave up before the panel came back. */
  | { phase: 'slow'; bots: number }
  /** The update request itself was refused or errored. */
  | { phase: 'failed'; error: string; bots: number }

/** On the new panel, what became of the bots the old page handed over. */
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

export const PANEL_UPDATE_STEPS = ['Download', 'Restart', 'Reconnect'] as const
const WITH_BOTS_STEPS = [...PANEL_UPDATE_STEPS, 'Update bots'] as const
const BOTS_STEP = WITH_BOTS_STEPS.length - 1

export function panelUpdateSteps(bots: number): readonly string[] {
  return bots > 0 ? WITH_BOTS_STEPS : PANEL_UPDATE_STEPS
}

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
  /** The one button worth offering next to the message. */
  action: 'reload' | 'fleet' | null
  dismissible: boolean
}

export function panelUpdateView(u: PanelUpdate): PanelUpdateView | null {
  switch (u.phase) {
    case 'idle':
      return null
    case 'pulling':
      return progress(
        u.bots,
        0,
        u.bots > 0
          ? "Downloading the update. Your bots keep trading, and update once Fleet is back."
          : 'Downloading the update. Your bots keep trading.',
      )
    case 'armed':
      return progress(u.bots, 1, 'Fleet restarts in a moment. This page goes quiet for a few seconds.')
    case 'restarting':
      return progress(
        u.bots,
        1,
        u.bots > 0
          ? 'Your bots keep trading. This page comes back by itself, then the bots update.'
          : 'Your bots keep trading. This page comes back by itself.',
      )
    case 'reloading':
      return progress(u.bots, 2, 'Fleet is back. Opening the new version.')
    case 'slow':
      return {
        ...base(u.bots),
        tone: 'warning',
        title: 'Fleet is taking a while',
        detail:
          "It isn't back yet. Your bots keep trading. Reload in a moment." +
          (u.bots > 0 ? " The bot updates start once it's back." : ''),
        action: 'reload',
      }
    case 'failed':
      return {
        ...base(u.bots),
        tone: 'danger',
        title: "Fleet update didn't start",
        detail: u.bots > 0 ? `${u.error} Your bots weren't touched.` : u.error,
        dismissible: true,
      }
  }
}

/**
 * The strip on the new panel, from the handover and the bot run it started.
 * `onFleet` is true on the Fleet page, where the run's own card lists each bot.
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
        ...base(a.bots),
        tone: 'progress',
        title: 'Fleet updated',
        detail: `${fleetNow(a.from, a.version)} Checking which bots still need the update.`,
        step: BOTS_STEP,
      }
    case 'bots':
      return run ? botsView(a, run, onFleet) : null
    case 'skipped':
      return skippedView(a)
  }
}

function botsView(
  a: { from: string; version: string },
  run: FleetUpdateRun,
  onFleet: boolean,
): PanelUpdateView {
  const c = runCounts(run)
  const now = fleetNow(a.from, a.version)
  if (runActive(run)) {
    return {
      ...base(c.total),
      tone: 'progress',
      title: `Fleet updated · bots ${c.settled} of ${c.total}${c.failed > 0 ? ` · ${c.failed} failed` : ''}`,
      detail:
        run.phase === 'stopping'
          ? 'Finishing the current bot, then stopping.'
          : `${now} Your bots update one at a time${onFleet ? ', below' : ''}. Keep this tab open until they're done.`,
      step: BOTS_STEP,
      action: 'fleet',
    }
  }
  if (c.failed > 0) {
    return {
      ...base(c.total),
      tone: 'danger',
      title: `Fleet updated · ${c.failed} of ${botCount(c.total)} didn't come back`,
      detail: failedDetail(c.done, c.failed, onFleet),
      action: 'fleet',
      dismissible: true,
    }
  }
  if (run.phase === 'cancelled') {
    return {
      ...base(c.total),
      tone: 'warning',
      title: 'Fleet updated · bot updates stopped',
      detail: `${c.done} of ${botCount(c.total)} updated before you stopped. The rest stay on their current version.`,
      action: 'fleet',
      dismissible: true,
    }
  }
  return {
    ...base(c.total),
    tone: 'success',
    title: `Fleet and ${botCount(c.total)} updated`,
    detail: `${now} Every bot is back up on the latest version.`,
    step: WITH_BOTS_STEPS.length,
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
  const done = { ...base(0), dismissible: true }
  switch (a.reason) {
    case 'panel-not-moved':
      return {
        ...done,
        tone: 'warning',
        title: "Fleet didn't update",
        detail:
          "It came back on the same version, so your bots weren't touched. Try Update again in a few minutes.",
      }
    case 'registry':
      return {
        ...done,
        tone: 'warning',
        title: 'Fleet updated, bots not checked',
        detail: `${now} We couldn't check for new bot versions, so no bots were updated. Try Update all in a few minutes.`,
        action: 'fleet',
      }
    case 'check-failed':
      return {
        ...done,
        tone: 'warning',
        title: 'Fleet updated, bots not checked',
        detail: `${now} We couldn't load your bots, so none were updated. Try Update all in a few minutes.`,
        action: 'fleet',
      }
    case 'expired':
      return {
        ...done,
        tone: 'warning',
        title: 'Bot updates not started',
        detail: `They were queued more than ${Math.round(HANDOFF_TTL_MS / 60_000)} minutes ago, so they didn't start. If your bots still need it, use Update all.`,
        action: 'fleet',
      }
    case 'nothing-behind':
      return {
        ...done,
        tone: 'success',
        title: 'Fleet updated',
        detail: `${now} Your bots were already up to date.`,
      }
  }
}

/** True while an update is under way and the header button should stay busy. */
export function panelUpdateBusy(u: PanelUpdate): boolean {
  return u.phase !== 'idle' && u.phase !== 'failed' && u.phase !== 'slow'
}

/**
 * True while the panel is swapping or deciding which bots to hand to the run.
 * Update all stays out of the way then: the panel shouldn't be recreating bots
 * while it's being replaced, and the handover is about to start a run itself.
 */
export function panelFlowBusy(u: PanelUpdate, a: AfterPanel): boolean {
  return panelUpdateBusy(u) || a.phase === 'checking'
}

function progress(bots: number, step: number, detail: string): PanelUpdateView {
  return {
    ...base(bots),
    tone: 'progress',
    title: bots > 0 ? `Updating Fleet and ${botCount(bots)}` : 'Updating Fleet',
    detail,
    step,
  }
}

function base(bots: number): Omit<PanelUpdateView, 'tone' | 'title' | 'detail'> {
  return {
    steps: panelUpdateSteps(bots),
    step: null,
    action: null,
    dismissible: false,
  }
}

function botCount(n: number): string {
  return `${n} ${n === 1 ? 'bot' : 'bots'}`
}

/** "Fleet is now on v0.1.240 (was v0.1.233).", or without the "was" when it held. */
export function fleetNow(from: string, to: string): string {
  return from && from !== to ? `Fleet is now on v${to} (was v${from}).` : `Fleet is now on v${to}.`
}

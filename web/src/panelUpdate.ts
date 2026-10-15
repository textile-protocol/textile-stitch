// What the header strip says while the panel updates itself.
//
// The update is three things the operator waits through: the panel pulls the
// new image, the container is swapped (this page loses its connection), and
// the page reloads onto the new build. Each phase maps to one of those steps,
// plus the two ways it can end badly: the request failed, or the panel hasn't
// come back in the time we're willing to poll.

import { shortImage } from './format'

export type PanelUpdate =
  | { phase: 'idle' }
  /** POST /api/panel/update is in flight: the panel is pulling the image. */
  | { phase: 'pulling'; target: string | null }
  /** Swap is scheduled; the old panel still answers. */
  | { phase: 'armed'; target: string | null }
  /** A session poll failed, so the container is being recreated. */
  | { phase: 'restarting'; target: string | null }
  /** The panel answered again; the page is reloading onto the new UI. */
  | { phase: 'reloading'; target: string | null }
  /** Polling gave up before the panel came back. */
  | { phase: 'slow'; target: string | null }
  /** The update request itself was refused or errored. */
  | { phase: 'failed'; error: string }

export const PANEL_UPDATE_STEPS = ['Pull image', 'Restart', 'Reload'] as const

export interface PanelUpdateView {
  tone: 'progress' | 'warning' | 'danger'
  title: string
  detail: string
  /** Short image ref the update lands on, when known. */
  target: string | null
  /** Index into PANEL_UPDATE_STEPS of the step under way; null hides the steps. */
  step: number | null
}

export function panelUpdateView(u: PanelUpdate): PanelUpdateView | null {
  switch (u.phase) {
    case 'idle':
      return null
    case 'pulling':
      return progress(u.target, 0, 'Pulling the new image. Bots keep running.')
    case 'armed':
      return progress(
        u.target,
        1,
        'The panel restarts in a moment. This page will drop out briefly.',
      )
    case 'restarting':
      return progress(
        u.target,
        1,
        "Bots keep running. This page reloads itself when the panel's back.",
      )
    case 'reloading':
      return progress(u.target, 2, 'Panel is back. Loading the new version.')
    case 'slow':
      return {
        tone: 'warning',
        title: 'Panel is taking a while',
        detail:
          "It hasn't come back yet. Bots keep running. Reload in a moment, or check the container if it stays down.",
        target: targetLabel(u.target),
        step: null,
      }
    case 'failed':
      return {
        tone: 'danger',
        title: "Panel update didn't start",
        detail: u.error,
        target: null,
        step: null,
      }
  }
}

/** True while an update is under way and the header button should stay busy. */
export function panelUpdateBusy(u: PanelUpdate): boolean {
  return u.phase !== 'idle' && u.phase !== 'failed' && u.phase !== 'slow'
}

function progress(target: string | null, step: number, detail: string): PanelUpdateView {
  return {
    tone: 'progress',
    title: 'Updating the panel',
    detail,
    target: targetLabel(target),
    step,
  }
}

function targetLabel(target: string | null): string | null {
  return target ? shortImage(target) : null
}

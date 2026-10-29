// The panel's self-update: the hook that drives it and the strip under the
// header that shows where it's at. Copy and step mapping live in
// ../panelUpdate.ts so they can be tested without React.

import { useState } from 'react'
import { ApiError, api } from '../api'
import {
  PANEL_UPDATE_STEPS,
  panelUpdateView,
  type PanelUpdate,
  type PanelUpdateView,
} from '../panelUpdate'
import { Spinner } from './ui'

const POLL_MS = 2000
const POLL_ATTEMPTS = 60
/** How long a healthy session counts as "not restarted yet" rather than "back". */
const GRACE_MS = 8_000

export function usePanelUpdate() {
  const [state, setState] = useState<PanelUpdate>({ phase: 'idle' })

  async function start(target: string | null) {
    if (
      !window.confirm(
        `Update the panel itself to ${target ?? 'the latest image'}?\n\nThe UI will disconnect briefly while the container is recreated. Bots keep running.`,
      )
    ) {
      return
    }
    setState({ phase: 'pulling', target })
    try {
      const res = await api.updatePanel()
      const next = res.targetImage || target
      setState({ phase: 'armed', target: next })
      // The helper sleeps ~2s then stop/rm/rename/start. Prefer seeing a failed
      // poll (the restart gap), but a lightweight panel can finish the swap
      // between two 2s polls — every session() then succeeds and sawDown never
      // flips. After a short grace, accept a healthy session as "back".
      const started = Date.now()
      let sawDown = false
      for (let i = 0; i < POLL_ATTEMPTS; i++) {
        await new Promise((r) => setTimeout(r, POLL_MS))
        try {
          await api.session()
          if (!sawDown && Date.now() - started < GRACE_MS) continue
          // Soft API refreshes leave the browser on the old embedded SPA. The new
          // container serves a fresh index.html (no-cache) + hashed assets — reload
          // so version, shell, and UI all match the image we just swapped to.
          setState({ phase: 'reloading', target: next })
          window.location.reload()
          return
        } catch {
          sawDown = true
          setState({ phase: 'restarting', target: next })
        }
      }
      setState({ phase: 'slow', target: next })
    } catch (e) {
      setState({ phase: 'failed', error: e instanceof ApiError ? e.message : String(e) })
    }
  }

  return { state, start, dismiss: () => setState({ phase: 'idle' }) }
}

const TONES: Record<PanelUpdateView['tone'], string> = {
  progress: 'bg-accent-tint',
  warning: 'bg-warning-bg',
  danger: 'bg-danger-bg',
}

const TITLE_TONES: Record<PanelUpdateView['tone'], string> = {
  progress: 'text-ink',
  warning: 'text-warning',
  danger: 'text-danger',
}

/**
 * Full-width strip under the header. Progress reads as a tinted band with the
 * three steps; a failure or a panel that hasn't come back switches tone and
 * offers the one thing worth doing next.
 */
export default function PanelUpdateStatus({
  state,
  onDismiss,
}: {
  state: PanelUpdate
  onDismiss: () => void
}) {
  const view = panelUpdateView(state)
  if (!view) return null

  return (
    <div
      role="status"
      aria-live="polite"
      className={`border-t border-line-soft ${TONES[view.tone]}`}
    >
      <div className="mx-auto flex max-w-[96rem] flex-col gap-3 px-4 py-3 sm:flex-row sm:items-center sm:gap-6 sm:px-6">
        <div className="flex min-w-0 flex-1 items-start gap-3">
          <StatusIcon tone={view.tone} />
          <div className="min-w-0 flex-1">
            <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
              <span className={`text-sm font-bold ${TITLE_TONES[view.tone]}`}>
                {view.title}
              </span>
              {view.target && (
                <span className="hidden rounded-md border border-line-soft bg-surface px-1.5 py-0.5 font-mono text-xs text-muted sm:inline">
                  {view.target}
                </span>
              )}
            </div>
            <p className="mt-0.5 text-sm text-muted [overflow-wrap:anywhere]">{view.detail}</p>
          </div>
          {view.tone === 'danger' && (
            <button
              type="button"
              onClick={onDismiss}
              aria-label="Dismiss"
              className="-mr-1 inline-flex size-7 shrink-0 items-center justify-center rounded-md text-muted hover:bg-hover hover:text-ink"
            >
              ✕
            </button>
          )}
        </div>
        {view.step !== null && <Steps current={view.step} />}
        {view.tone === 'warning' && (
          <button
            type="button"
            onClick={() => window.location.reload()}
            className="ml-8 self-start rounded-lg border border-line bg-surface px-3 py-1.5 text-sm font-bold hover:bg-hover sm:ml-0 sm:self-auto"
          >
            Reload now
          </button>
        )}
      </div>
    </div>
  )
}

function StatusIcon({ tone }: { tone: PanelUpdateView['tone'] }) {
  if (tone === 'progress') {
    return (
      <span className="mt-0.5 inline-flex size-5 shrink-0 items-center justify-center text-accent">
        <Spinner />
      </span>
    )
  }
  const color = tone === 'danger' ? 'bg-danger' : 'bg-warning'
  return (
    <span
      aria-hidden
      className={`mt-0.5 inline-flex size-5 shrink-0 items-center justify-center rounded-full text-xs font-bold text-surface ${color}`}
    >
      !
    </span>
  )
}

/** Pull image → Restart → Reload. Done steps tick, the current one fills. */
function Steps({ current }: { current: number }) {
  return (
    <ol className="flex items-center gap-2 pl-8 text-xs sm:pl-0">
      {PANEL_UPDATE_STEPS.map((label, i) => {
        const done = i < current
        const active = i === current
        return (
          <li key={label} className="flex items-center gap-2">
            {i > 0 && (
              <span
                aria-hidden
                className={`h-px w-4 sm:w-6 ${done || active ? 'bg-accent' : 'bg-line'}`}
              />
            )}
            <span
              aria-hidden
              className={`inline-flex size-4 items-center justify-center rounded-full text-[10px] font-bold ${
                done
                  ? 'bg-accent text-on-accent'
                  : active
                    ? 'border-2 border-accent bg-surface'
                    : 'border border-line bg-surface'
              }`}
            >
              {done ? '✓' : ''}
            </span>
            <span
              aria-current={active ? 'step' : undefined}
              className={`whitespace-nowrap ${
                active ? 'font-bold text-ink' : done ? 'text-muted' : 'text-faint'
              }`}
            >
              {label}
            </span>
          </li>
        )
      })}
    </ol>
  )
}

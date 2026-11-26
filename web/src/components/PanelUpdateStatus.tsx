// The strip under the header that shows where a panel update is at, and the
// bot updates after it. Copy and step mapping live in ../panelUpdate.ts so
// they can be tested without React; the stores are in ../usePanelUpdate.ts.

import { Link } from 'react-router-dom'
import type { PanelUpdateView } from '../panelUpdate'
import { Spinner } from './ui'

const TONES: Record<PanelUpdateView['tone'], string> = {
  progress: 'bg-accent-tint',
  success: 'bg-success-bg',
  warning: 'bg-warning-bg',
  danger: 'bg-danger-bg',
}

const TITLE_TONES: Record<PanelUpdateView['tone'], string> = {
  progress: 'text-ink',
  success: 'text-success',
  warning: 'text-warning',
  danger: 'text-danger',
}

const ACTION =
  'shrink-0 rounded-lg border border-line bg-surface px-3 py-1.5 text-sm font-bold hover:bg-hover'

/**
 * Full-width strip under the header. Progress reads as a tinted band with the
 * steps; an outcome switches tone and offers the one thing worth doing next.
 * `showFleetLink` is false on the Fleet page itself, where the run's own card is.
 */
export default function PanelUpdateStatus({
  view,
  showFleetLink,
  onDismiss,
  onUpdateFleetUi,
}: {
  view: PanelUpdateView | null
  showFleetLink: boolean
  onDismiss: () => void
  /** Go on with Fleet UI after the bots were held. */
  onUpdateFleetUi: () => void
}) {
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
            <div className={`text-sm font-bold ${TITLE_TONES[view.tone]}`}>{view.title}</div>
            <p className="mt-0.5 text-sm text-muted [overflow-wrap:anywhere]">{view.detail}</p>
          </div>
          {view.dismissible && <Dismiss onClick={onDismiss} className="sm:hidden" />}
        </div>
        {view.step !== null && (
          <Steps labels={view.steps} current={view.step} success={view.tone === 'success'} />
        )}
        {view.actions.length > 0 && (
          <div className="ml-8 flex shrink-0 flex-wrap gap-2 sm:ml-0">
            {view.actions.includes('reload') && (
              <button type="button" onClick={() => window.location.reload()} className={ACTION}>
                Reload now
              </button>
            )}
            {view.actions.includes('fleet') && showFleetLink && (
              <Link to="/" className={ACTION}>
                View bots
              </Link>
            )}
            {view.actions.includes('fleet-ui') && (
              <button type="button" onClick={onUpdateFleetUi} className={ACTION}>
                Update Fleet UI
              </button>
            )}
          </div>
        )}
        {view.dismissible && <Dismiss onClick={onDismiss} className="max-sm:hidden" />}
      </div>
    </div>
  )
}

/** In the text row on a phone, at the end of the strip from `sm` up. */
function Dismiss({ onClick, className }: { onClick: () => void; className: string }) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-label="Dismiss"
      className={`-mr-1 inline-flex size-7 shrink-0 items-center justify-center rounded-md text-muted hover:bg-hover hover:text-ink ${className}`}
    >
      ✕
    </button>
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
  const look = {
    success: { color: 'bg-success', glyph: '✓' },
    warning: { color: 'bg-warning', glyph: '!' },
    danger: { color: 'bg-danger', glyph: '!' },
  }[tone]
  return (
    <span
      aria-hidden
      className={`mt-0.5 inline-flex size-5 shrink-0 items-center justify-center rounded-full text-xs font-bold text-surface ${look.color}`}
    >
      {look.glyph}
    </span>
  )
}

/** Download → Restart → Reconnect (→ Update bots). Done steps tick, the current one fills. */
function Steps({
  labels,
  current,
  success,
}: {
  labels: readonly string[]
  current: number
  /** Everything ticked on a finished flow: green, like the band it sits on. */
  success: boolean
}) {
  const fill = success ? 'bg-success text-surface' : 'bg-accent text-on-accent'
  const line = success ? 'bg-success' : 'bg-accent'
  return (
    <ol className="flex flex-wrap items-center gap-2 pl-8 text-xs sm:flex-nowrap sm:pl-0">
      {labels.map((label, i) => {
        const done = i < current
        const active = i === current
        return (
          <li key={label} className="flex items-center gap-2">
            {i > 0 && (
              <span
                aria-hidden
                className={`h-px w-4 sm:w-6 ${done || active ? line : 'bg-line'}`}
              />
            )}
            <span
              aria-hidden
              className={`inline-flex size-4 items-center justify-center rounded-full text-[10px] font-bold ${
                done
                  ? fill
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

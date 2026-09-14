// The "what happens now" list shown once the wallet is funded: the whole path
// is visible at once, with each row pending, running, done, failed or skipped.
// Shared by the Approve step and the Confirm-your-email screen.

import type { ReactNode } from 'react'
import { Spinner } from '../ui'
import type { ProgressState } from './useStartSequence'

export interface ProgressRow {
  key: string
  title: string
  sub?: string
  state: ProgressState
  /** Extra content under the row, e.g. the approve output. */
  children?: ReactNode
}

export default function ProgressList({ rows }: { rows: ProgressRow[] }) {
  return (
    <ol className="space-y-2">
      {rows.map((row) => (
        <li
          key={row.key}
          className="flex items-start gap-3 rounded-lg border border-line-soft px-3 py-2.5"
        >
          <Glyph state={row.state} />
          <div className="min-w-0 flex-1">
            <p
              className={`text-sm ${
                row.state === 'pending'
                  ? 'text-muted'
                  : row.state === 'failed'
                    ? 'font-bold text-danger'
                    : 'font-bold text-ink'
              }`}
            >
              {row.title}
            </p>
            {row.sub && <p className="mt-0.5 text-xs text-faint">{row.sub}</p>}
            {row.children}
          </div>
        </li>
      ))}
    </ol>
  )
}

function Glyph({ state }: { state: ProgressState }) {
  const base = 'mt-0.5 flex size-5 shrink-0 items-center justify-center text-sm'
  switch (state) {
    case 'running':
      return (
        <span className={`${base} text-accent`}>
          <Spinner />
        </span>
      )
    case 'done':
      return (
        <span className={`${base} font-bold text-success`} aria-label="done">
          ✓
        </span>
      )
    case 'skipped':
      return (
        <span className={`${base} text-muted`} title="Already done" aria-label="already done">
          ✓
        </span>
      )
    case 'failed':
      return (
        <span className={`${base} font-bold text-danger`} aria-label="failed">
          ✕
        </span>
      )
    default:
      return (
        <span className={`${base} text-faint`} aria-label="pending">
          ○
        </span>
      )
  }
}

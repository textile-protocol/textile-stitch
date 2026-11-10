import { Link } from 'react-router-dom'
import { botPath } from '../botRoutes'
import { shortImage } from '../format'
import { type FleetUpdateRun, type UpdateStep, runActive, runCounts } from '../fleetUpdate'
import { Button, Spinner } from './ui'

/**
 * The Update all run on the Fleet page: one line per bot while it goes, and a
 * summary when it ends. Failed bots are listed first in the summary, each with
 * a link to its page, so the ones that need a look aren't lost among the rest.
 */
export default function FleetUpdateProgress({
  run,
  onStop,
  onDismiss,
}: {
  run: FleetUpdateRun
  onStop: () => void
  onDismiss: () => void
}) {
  const counts = runCounts(run)
  const active = runActive(run)
  const failures = run.steps.filter((s) => s.status === 'failed')
  const untouched = run.steps.filter((s) => s.status === 'untouched').length
  const tone = active
    ? 'border-line-soft'
    : failures.length > 0
      ? 'border-danger/40'
      : run.phase === 'cancelled'
        ? 'border-warning/40'
        : 'border-success/40'
  // Run order while it goes; failures on top once it's over.
  const steps = active
    ? run.steps
    : [...failures, ...run.steps.filter((s) => s.status !== 'failed')]

  return (
    <section className={`bg-surface rounded-xl border ${tone}`} aria-live="polite">
      <header className="flex flex-wrap items-start justify-between gap-3 px-4 pt-4 pb-3">
        <div>
          <h2 className="text-base font-bold">{headline(run, counts)}</h2>
          <p className="text-muted text-sm">{subline(run, failures, untouched)}</p>
        </div>
        {active ? (
          <Button onClick={onStop} disabled={run.phase === 'stopping'}>
            {run.phase === 'stopping' ? 'Stopping after this bot' : 'Stop after this bot'}
          </Button>
        ) : (
          <Button variant="ghost" onClick={onDismiss}>
            Dismiss
          </Button>
        )}
      </header>
      {active && <ProgressBar settled={counts.settled} total={counts.total} />}
      <ol className="divide-line-soft border-line-soft divide-y border-t">
        {steps.map((s) => (
          <StepRow key={s.name} step={s} />
        ))}
      </ol>
    </section>
  )
}

function headline(run: FleetUpdateRun, c: ReturnType<typeof runCounts>): string {
  const target = shortImage(run.target)
  const bots = (n: number) => `${n} ${n === 1 ? 'bot' : 'bots'}`
  if (runActive(run)) {
    const failed = c.failed > 0 ? ` · ${c.failed} failed` : ''
    return `Updating bots to ${target} · ${c.settled} of ${c.total}${failed}`
  }
  if (c.failed === 0 && run.phase === 'done') return `Updated ${bots(c.total)} to ${target}`
  const failed = c.failed > 0 ? `, ${c.failed} failed` : ''
  return `Updated ${c.done} of ${bots(c.total)} to ${target}${failed}`
}

function subline(run: FleetUpdateRun, failures: UpdateStep[], untouched: number): string {
  if (run.phase === 'running') {
    return 'One at a time. You can open a bot meanwhile; keep this tab open until it finishes.'
  }
  if (run.phase === 'stopping') return 'Finishing the bot in flight, then stopping.'
  const parts = [
    failures.length === 1 && `${failures[0]?.label} didn't come back up. Open it to read its logs or roll it back.`,
    failures.length > 1 && "These didn't come back up. Open each to read its logs or roll it back.",
    run.phase === 'cancelled' &&
      `You stopped the run. ${untouched} ${untouched === 1 ? 'bot was' : 'bots were'} left on the build ${untouched === 1 ? 'it' : 'they'} had.`,
  ].filter(Boolean)
  return parts.length > 0 ? parts.join(' ') : 'Each one came back up on the new image.'
}

function ProgressBar({ settled, total }: { settled: number; total: number }) {
  return (
    <div className="bg-hover mx-4 mb-3 h-1.5 overflow-hidden rounded-full">
      <div
        className="bg-accent h-full rounded-full transition-all"
        style={{ width: `${total === 0 ? 0 : (settled / total) * 100}%` }}
      />
    </div>
  )
}

function StepRow({ step }: { step: UpdateStep }) {
  const { icon, text, tone } = stepView(step)
  return (
    <li className="flex items-start gap-3 px-4 py-2.5">
      <span className={`flex h-5 w-4 shrink-0 items-center justify-center text-sm ${tone}`}>
        {icon}
      </span>
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-baseline justify-between gap-x-3">
          <span className="text-sm font-bold">{step.label}</span>
          <span className={`text-xs ${tone}`}>
            {step.status === 'failed' ? (
              <Link to={botPath(step.name)} className="font-bold underline">
                Open {step.label}
              </Link>
            ) : (
              text
            )}
          </span>
        </div>
        {step.status === 'failed' && step.detail && (
          <p className="text-danger mt-1 text-xs">{step.detail}</p>
        )}
      </div>
    </li>
  )
}

function stepView(step: UpdateStep): { icon: React.ReactNode; text: string; tone: string } {
  switch (step.status) {
    case 'queued':
      return { icon: '·', text: 'Queued', tone: 'text-faint' }
    case 'updating':
      return { icon: <Spinner />, text: 'Pulling and recreating', tone: 'text-accent' }
    case 'checking':
      return { icon: <Spinner />, text: 'Waiting for it to stay up', tone: 'text-accent' }
    case 'done':
      return {
        icon: '✓',
        text: step.wantsUp ? 'Updated, running' : 'Updated, left stopped',
        tone: 'text-success',
      }
    case 'failed':
      return { icon: '✕', text: 'Failed', tone: 'text-danger' }
    case 'untouched':
      return { icon: '–', text: 'Not updated', tone: 'text-faint' }
  }
}

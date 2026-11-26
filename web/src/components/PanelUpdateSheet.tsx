import { useEffect, useRef, useState } from 'react'
import { type FleetUpdatePlan, type UpdateCandidate, defaultSelection } from '../fleetUpdate'
import { Button, Spinner } from './ui'
import { CandidateRow } from './UpdateAllSheet'

/** What the sheet knows about the bots when it opens. */
export type BotsSection =
  | { state: 'loading' }
  | { state: 'error'; error: string }
  | { state: 'ready'; plan: FleetUpdatePlan }

/**
 * The confirm step for Update, in the order things happen: the bots first,
 * one at a time, then Fleet UI (the panel itself). The operator is assumed to
 * want every bot that's behind updated: one "Stitch bots" switch, on by
 * default, with the per-bot list folded away for whoever wants to leave one
 * out. What's ticked here is exactly what the run updates. `bots` is null
 * where bulk bot updates aren't offered.
 *
 * The copy is for operators who think in trades and balances: no images,
 * containers or registries.
 */
export default function PanelUpdateSheet({
  open,
  panelTarget,
  version,
  bots,
  signsOut,
  onClose,
  onStart,
}: {
  open: boolean
  panelTarget: string
  /** The panel's version now. */
  version: string
  bots: BotsSection | null
  /** A password session doesn't survive the panel's restart. */
  signsOut: boolean
  onClose: () => void
  /** The ticked bots, in run order; empty for Fleet UI alone. */
  onStart: (picked: UpdateCandidate[]) => void
}) {
  const ref = useRef<HTMLDialogElement>(null)
  const plan = bots?.state === 'ready' ? bots.plan : null
  const [withBots, setWithBots] = useState(true)
  const [picked, setPicked] = useState<ReadonlySet<string>>(() => seed(plan))
  // The plan can land after the sheet opens. Tick its defaults once, when it
  // does, so a later fleet read never unticks what the operator unticked.
  const seeded = useRef(plan !== null)

  useEffect(() => {
    const dialog = ref.current
    if (open && dialog && !dialog.open) {
      seeded.current = plan !== null
      setWithBots(true)
      setPicked(seed(plan))
      dialog.showModal()
    }
    // Reset on open only; the effect below handles a plan that lands later.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  useEffect(() => {
    if (open && plan && !seeded.current) {
      seeded.current = true
      setPicked(seed(plan))
    }
  }, [open, plan])

  if (!open) return null

  const ready = plan?.ready ?? []
  const chosen = withBots ? ready.filter((c) => picked.has(c.name)) : []
  const loading = bots?.state === 'loading'
  const toggle = (name: string) =>
    setPicked((s) => {
      const next = new Set(s)
      if (next.has(name)) next.delete(name)
      else next.add(name)
      return next
    })
  const close = () => ref.current?.close()

  return (
    <dialog
      ref={ref}
      aria-label="Update Stitch Fleet"
      onClose={onClose}
      onClick={(e) => {
        if (e.target === e.currentTarget) close()
      }}
      className="border-line bg-surface text-ink m-auto w-full max-w-xl rounded-xl border p-0 backdrop:bg-black/50"
    >
      <header className="border-line-soft flex items-start justify-between gap-4 border-b px-5 py-4">
        <div>
          <h2 className="text-base font-bold">Update Stitch Fleet</h2>
          <p className="text-muted mt-0.5 text-sm">
            {bots ? 'Your bots first, then Fleet UI.' : 'Your bots keep trading while it updates.'}
          </p>
        </div>
        <button
          type="button"
          onClick={close}
          aria-label="Close"
          className="text-muted hover:bg-hover rounded-lg px-2 py-1"
        >
          ✕
        </button>
      </header>

      <div className="max-h-[65vh] space-y-3 overflow-y-auto px-5 py-4">
        {bots && (
          <BotsStage
            bots={bots}
            withBots={withBots}
            onWithBots={setWithBots}
            picked={picked}
            onToggle={toggle}
            count={chosen.length}
          />
        )}

        <Stage
          n={bots ? 2 : 1}
          title="Fleet UI"
          aside={`v${version} → ${imageTag(panelTarget)}`}
          asideTitle={panelTarget}
        >
          {chosen.length > 0
            ? 'Updates once your bots are done. Takes a few seconds: your bots keep trading, and this page comes back by itself.'
            : 'Takes a few seconds. Your bots keep trading, and this page comes back by itself.'}
        </Stage>
      </div>

      <footer className="border-line-soft flex flex-wrap items-center justify-between gap-3 border-t px-5 py-4">
        <p className="text-muted max-w-xs text-xs">
          {chosen.length > 0
            ? "Keep this tab open until it's done. If a bot doesn't come back up, Fleet UI waits so you can fix it first." +
              (signsOut ? " You'll sign in again at the end." : '')
            : signsOut
              ? "Your bots stay on their current version. You'll sign in again after the restart."
              : 'Your bots stay on their current version. You can update them later.'}
        </p>
        <div className="ml-auto flex gap-2">
          <Button onClick={close}>Cancel</Button>
          <Button
            variant="primary"
            disabled={loading}
            onClick={() => {
              onStart(chosen)
              close()
            }}
          >
            {chosen.length > 0
              ? `Update ${chosen.length} ${chosen.length === 1 ? 'bot' : 'bots'} and Fleet UI`
              : 'Update Fleet UI'}
          </Button>
        </div>
      </footer>
    </dialog>
  )
}

/** `latest` from `ghcr.io/…/textile-stitch-panel:latest`: the channel it moves to. */
function imageTag(image: string): string {
  const name = image.slice(image.lastIndexOf('/') + 1)
  const colon = name.indexOf(':')
  return colon === -1 ? 'latest' : name.slice(colon + 1)
}

/** "Your bot updates" / "Both bots update" / "All 4 bots update". */
function allBots(n: number): string {
  if (n === 1) return 'Your bot updates'
  return n === 2 ? 'Both bots update' : `All ${n} bots update`
}

function seed(plan: FleetUpdatePlan | null): ReadonlySet<string> {
  return new Set(plan ? defaultSelection(plan) : [])
}

function BotsStage({
  bots,
  withBots,
  onWithBots,
  picked,
  onToggle,
  count,
}: {
  bots: BotsSection
  withBots: boolean
  onWithBots: (on: boolean) => void
  picked: ReadonlySet<string>
  onToggle: (name: string) => void
  count: number
}) {
  if (bots.state === 'loading') {
    return (
      <Stage n={1} title="Stitch bots" muted>
        <span className="inline-flex items-center gap-2">
          <Spinner /> Checking which bots need an update…
        </span>
      </Stage>
    )
  }
  if (bots.state === 'error') {
    return (
      <Stage n={1} title="Stitch bots" muted>
        <span title={bots.error}>
          We couldn&rsquo;t load your bots, so only Fleet UI will update. You can update them
          afterwards with Update all.
        </span>
      </Stage>
    )
  }
  const { plan } = bots
  const total = plan.ready.length
  if (total === 0) {
    return (
      <Stage n={1} title="Stitch bots" muted aside={plan.skipped.length > 0 ? undefined : 'Up to date'}>
        {plan.skipped.length > 0
          ? "The bots that need an update can't be updated from here."
          : 'All your bots are on the latest version.'}
        <Skipped plan={plan} />
      </Stage>
    )
  }

  return (
    <section className="border-line-soft rounded-lg border">
      <div className="flex items-start gap-3 px-4 py-3">
        <StageNumber n={1} muted={!withBots} />
        <div className="min-w-0 flex-1">
          <div className="flex items-center justify-between gap-3">
            <span className="text-sm font-bold">
              Stitch bots
            </span>
            <Switch checked={withBots} onChange={onWithBots} label="Update the Stitch bots too" />
          </div>
          <p className="text-muted mt-0.5 text-xs">
            {!withBots
              ? 'Your bots stay on their current version.'
              : count === total
                ? `${allBots(total)} one at a time. Each pauses trading for a few seconds and has to come back up before the next.`
                : `${count} of ${total} bots update one at a time. Each pauses trading for a few seconds and has to come back up before the next.`}
          </p>
        </div>
      </div>
      {withBots && (
        <details className="border-line-soft group border-t">
          <summary className="text-muted hover:text-ink flex cursor-pointer list-none items-center justify-between gap-3 px-4 py-2.5 text-xs font-bold select-none [&::-webkit-details-marker]:hidden">
            <span className="flex items-center gap-1.5">
              <span aria-hidden className="inline-block transition-transform group-open:rotate-90">
                ›
              </span>
              Choose bots
            </span>
            <span className="font-normal">
              {count} of {total} selected
            </span>
          </summary>
          <ul className="border-line-soft divide-line-soft divide-y border-t">
            {plan.ready.map((c) => (
              <CandidateRow
                key={c.name}
                c={c}
                checked={picked.has(c.name)}
                onToggle={() => onToggle(c.name)}
              />
            ))}
          </ul>
          <Skipped plan={plan} framed />
        </details>
      )}
    </section>
  )
}

/** Bots that need an update but are left as they are, and why. */
function Skipped({ plan, framed = false }: { plan: FleetUpdatePlan; framed?: boolean }) {
  if (plan.skipped.length === 0) return null
  return (
    <div className={`text-xs ${framed ? 'border-line-soft border-t px-4 py-2.5' : 'mt-2'}`}>
      <p className="text-muted font-bold">Not updated ({plan.skipped.length})</p>
      <ul className="mt-1.5 space-y-1.5">
        {plan.skipped.map((s) => (
          <li key={s.name}>
            <span className="font-bold">{s.label}</span>
            <span className="text-muted"> · {s.reason}</span>
          </li>
        ))}
      </ul>
    </div>
  )
}

function Stage({
  n,
  title,
  aside,
  asideTitle,
  muted = false,
  children,
}: {
  n: number
  title: string
  aside?: string
  asideTitle?: string
  muted?: boolean
  children: React.ReactNode
}) {
  return (
    <section className="border-line-soft flex items-start gap-3 rounded-lg border px-4 py-3">
      <StageNumber n={n} muted={muted} />
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-baseline justify-between gap-x-3">
          <span className="text-sm font-bold">{title}</span>
          {aside && (
            <span title={asideTitle} className="text-muted text-xs">
              {aside}
            </span>
          )}
        </div>
        <div className="text-muted mt-0.5 text-xs">{children}</div>
      </div>
    </section>
  )
}

function StageNumber({ n, muted }: { n: number; muted: boolean }) {
  return (
    <span
      aria-hidden
      className={`mt-px inline-flex size-5 shrink-0 items-center justify-center rounded-full text-[11px] font-bold ${
        muted ? 'border-line text-faint border' : 'bg-accent text-on-accent'
      }`}
    >
      {n}
    </span>
  )
}

function Switch({
  checked,
  onChange,
  label,
}: {
  checked: boolean
  onChange: (on: boolean) => void
  /** What switching it on does: the card title alone doesn't say. */
  label: string
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      onClick={() => onChange(!checked)}
      className={`relative inline-flex h-5 w-9 shrink-0 items-center rounded-full transition-colors ${
        checked ? 'bg-accent' : 'bg-line'
      }`}
    >
      <span
        aria-hidden
        className={`inline-block size-4 rounded-full bg-white shadow-sm transition-transform ${
          checked ? 'translate-x-[18px]' : 'translate-x-0.5'
        }`}
      />
    </button>
  )
}

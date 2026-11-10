import { useEffect, useRef, useState } from 'react'
import { imageLabel, shortImage } from '../format'
import {
  type FleetUpdatePlan,
  type UpdateCandidate,
  defaultSelection,
} from '../fleetUpdate'
import { Button } from './ui'

/**
 * The confirm step for Update all: which bots move, which are left alone, and
 * why. Nothing runs until the operator presses the button, and the list is the
 * one the run uses, so what they ticked is exactly what gets recreated.
 */
export default function UpdateAllSheet({
  open,
  plan,
  target,
  onClose,
  onStart,
}: {
  open: boolean
  plan: FleetUpdatePlan
  target: string
  onClose: () => void
  onStart: (picked: UpdateCandidate[]) => void
}) {
  const ref = useRef<HTMLDialogElement>(null)
  const [picked, setPicked] = useState<ReadonlySet<string>>(() => new Set(defaultSelection(plan)))

  useEffect(() => {
    const dialog = ref.current
    if (open && dialog && !dialog.open) {
      setPicked(new Set(defaultSelection(plan)))
      dialog.showModal()
    }
    // The plan is read when the sheet opens, not while it's open: a fleet poll
    // mustn't untick what the operator just ticked.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  if (!open) return null

  const chosen = plan.ready.filter((c) => picked.has(c.name))
  const toggle = (name: string) =>
    setPicked((s) => {
      const next = new Set(s)
      if (next.has(name)) next.delete(name)
      else next.add(name)
      return next
    })

  return (
    <dialog
      ref={ref}
      aria-label="Update all bots"
      onClose={onClose}
      onClick={(e) => {
        if (e.target === e.currentTarget) ref.current?.close()
      }}
      className="border-line bg-surface text-ink m-auto w-full max-w-xl rounded-xl border p-0 backdrop:bg-black/50"
    >
      <header className="border-line-soft flex items-start justify-between gap-4 border-b px-5 py-4">
        <div>
          <h2 className="text-base font-bold">Update bots</h2>
          <p className="text-muted mt-0.5 text-sm">
            To <span className="font-mono">{shortImage(target)}</span>
          </p>
        </div>
        <button
          type="button"
          onClick={() => ref.current?.close()}
          aria-label="Close"
          className="text-muted hover:bg-hover rounded-lg px-2 py-1"
        >
          ✕
        </button>
      </header>

      <div className="max-h-[60vh] space-y-5 overflow-y-auto px-5 py-4">
        <p className="text-muted text-sm">
          One bot at a time: pull the image, recreate the container, wait for it to stay up,
          then the next. A running bot stops quoting for a few seconds while it's replaced.
          Config and keys stay on disk.
        </p>

        {plan.ready.length > 0 && (
          <Group title={`Behind (${plan.ready.length})`}>
            {plan.ready.map((c) => (
              <CandidateRow
                key={c.name}
                c={c}
                checked={picked.has(c.name)}
                onToggle={() => toggle(c.name)}
              />
            ))}
          </Group>
        )}

        {plan.skipped.length > 0 && (
          <Group title={`Left as they are (${plan.skipped.length})`}>
            {plan.skipped.map((s) => (
              <li key={s.name} className="flex gap-3 px-3 py-2.5">
                <span className="text-faint mt-0.5 w-4 shrink-0 text-center">–</span>
                <div className="min-w-0">
                  <div className="text-muted text-sm font-bold">{s.label}</div>
                  <div className="text-muted text-xs">{s.reason}</div>
                </div>
              </li>
            ))}
          </Group>
        )}
      </div>

      <footer className="border-line-soft flex flex-wrap items-center justify-between gap-3 border-t px-5 py-4">
        <p className="text-muted text-xs">
          If a bot doesn't come back up, the rest still update. Failures are listed at the end.
        </p>
        <div className="ml-auto flex gap-2">
          <Button onClick={() => ref.current?.close()}>Cancel</Button>
          <Button
            variant="primary"
            disabled={chosen.length === 0}
            onClick={() => {
              onStart(chosen)
              ref.current?.close()
            }}
          >
            Update {chosen.length} {chosen.length === 1 ? 'bot' : 'bots'}
          </Button>
        </div>
      </footer>
    </dialog>
  )
}

function Group({
  title,
  note,
  children,
}: {
  title: string
  note?: string
  children: React.ReactNode
}) {
  return (
    <section>
      <h3 className="text-faint mb-1.5 text-xs font-bold tracking-wide uppercase">{title}</h3>
      {note && <p className="text-muted mb-2 text-xs">{note}</p>}
      <ul className="border-line-soft divide-line-soft divide-y rounded-lg border">{children}</ul>
    </section>
  )
}

function CandidateRow({
  c,
  checked,
  onToggle,
}: {
  c: UpdateCandidate
  checked: boolean
  onToggle: () => void
}) {
  return (
    <li>
      <label className="hover:bg-hover flex cursor-pointer items-center gap-3 px-3 py-2.5">
        <input
          type="checkbox"
          checked={checked}
          onChange={onToggle}
          className="accent-[var(--tx-accent)] size-4 shrink-0"
        />
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-baseline gap-x-2">
            <span className="text-sm font-bold">{c.label}</span>
            {c.label !== c.name && (
              <span className="text-faint font-mono text-xs">{c.name}</span>
            )}
          </div>
          <div className="text-muted font-mono text-xs">{imageLabel(c.image, c.version)}</div>
        </div>
        <span className="text-muted shrink-0 text-xs">
          {c.wantsUp ? 'restarts' : 'stays stopped'}
        </span>
      </label>
    </li>
  )
}

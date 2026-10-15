import { useEffect, useId, useRef, useState, type ReactNode } from 'react'
import { percentage } from '../modulePresentation'

/** Shared module editor vocabulary: exact inputs + sliders, accessible switches,
 * and keyboard/touch disclosures. Values stay in the API's native units. */
export function Disclosure({
  title,
  children,
  className = '',
}: {
  title: string
  children: ReactNode
  className?: string
}) {
  return (
    <details
      className={`border-line-soft bg-surface group rounded-xl border ${className}`}
    >
      <summary className="flex cursor-pointer list-none items-center justify-between gap-3 p-4 text-sm font-bold [&::-webkit-details-marker]:hidden">
        <span>{title}</span>
        <span
          aria-hidden
          className="text-muted transition-transform group-open:rotate-90"
        >
          ›
        </span>
      </summary>
      <div className="border-line-soft border-t p-4">{children}</div>
    </details>
  )
}

/** Second-order settings, always visible. A caption and a recessed panel set
 * them apart from the module's main controls. */
export function SecondarySettings({
  title,
  children,
}: {
  title: string
  children: ReactNode
}) {
  const id = useId()
  return (
    <section aria-labelledby={id} className="space-y-2.5 pt-1">
      <h5
        id={id}
        className="text-faint text-[11px] font-bold uppercase tracking-wider"
      >
        {title}
      </h5>
      <div className="border-line-soft bg-canvas divide-line-soft divide-y rounded-xl border [&>*]:p-4">
        {children}
      </div>
    </section>
  )
}

export function Chevron({ className = '' }: { className?: string }) {
  return (
    <svg
      aria-hidden
      viewBox="0 0 12 12"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={`size-3 shrink-0 ${className}`}
    >
      <path d="M3 4.5 6 7.5 9 4.5" />
    </svg>
  )
}

/** A button with a small anchored panel. Closes on Escape (focus returns to
 * the button), on a press outside, and when focus tabs out. The panel follows
 * the button in tab order, so it needs no focus trap. */
export function Popover({
  label,
  buttonClassName,
  align = 'start',
  className = '',
  children,
}: {
  label: ReactNode
  buttonClassName: string
  align?: 'start' | 'end'
  className?: string
  children: (close: () => void) => ReactNode
}) {
  const [open, setOpen] = useState(false)
  const root = useRef<HTMLDivElement>(null)
  const button = useRef<HTMLButtonElement>(null)
  const id = useId()
  const close = () => {
    setOpen(false)
    button.current?.focus()
  }
  useEffect(() => {
    if (!open) return
    const outside = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false)
    }
    const escape = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      setOpen(false)
      button.current?.focus()
    }
    document.addEventListener('pointerdown', outside)
    document.addEventListener('keydown', escape)
    return () => {
      document.removeEventListener('pointerdown', outside)
      document.removeEventListener('keydown', escape)
    }
  }, [open])
  return (
    <div
      ref={root}
      className={`relative ${className}`}
      onBlur={(e) => {
        // A click on panel text blurs with no related target; only a focus
        // move outside closes.
        if (e.relatedTarget && !root.current?.contains(e.relatedTarget))
          setOpen(false)
      }}
    >
      <button
        ref={button}
        type="button"
        aria-expanded={open}
        aria-controls={open ? id : undefined}
        onClick={() => setOpen((value) => !value)}
        className={buttonClassName}
      >
        {label}
      </button>
      {open && (
        <div
          id={id}
          className={`border-line bg-surface absolute top-full z-30 mt-1.5 max-h-[min(34rem,70vh)] w-[min(23rem,calc(100vw-3rem))] overflow-y-auto rounded-xl border shadow-xl ${align === 'end' ? 'right-0' : 'left-0'}`}
        >
          {children(close)}
        </div>
      )}
    </div>
  )
}

export function ModuleSwitch({
  label,
  checked,
  onChange,
  showState = true,
}: {
  label: string
  checked: boolean
  onChange: (value: boolean) => void
  /** Off where the row already says whether the module is on. */
  showState?: boolean
}) {
  return (
    <label className="relative inline-flex cursor-pointer items-center gap-2 text-sm">
      <input
        type="checkbox"
        role="switch"
        aria-label={label}
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
        className="peer absolute left-0 top-0 z-10 h-6 w-10 cursor-pointer opacity-0"
      />
      <span
        aria-hidden
        className="bg-active after:bg-muted peer-checked:after:bg-on-accent relative h-6 w-10 rounded-full transition-colors after:absolute after:left-1 after:top-1 after:size-4 after:rounded-full after:transition-transform peer-checked:bg-accent peer-checked:after:translate-x-4 peer-focus-visible:outline peer-focus-visible:outline-2 peer-focus-visible:outline-offset-2 peer-focus-visible:outline-accent"
      />
      {showState && <span aria-hidden>{checked ? 'On' : 'Off'}</span>}
    </label>
  )
}

export function NumberControl({
  label,
  value,
  onChange,
  unit = '%',
  scale = 100,
  min = 0,
  max = 4999,
  step = 1,
  sliderMax,
  hint,
}: {
  label: string
  value: number
  onChange: (value: number) => void
  unit?: string
  scale?: number
  min?: number
  max?: number
  step?: number
  sliderMax?: number
  hint?: string
}) {
  const id = useId()
  const finite = Number.isFinite(value)
  const valid =
    finite &&
    value >= min &&
    value <= max &&
    (step !== 1 || Number.isInteger(value))
  const end = Math.min(max, Math.max(sliderMax ?? max, finite ? value : min))
  const fill = valid
    ? Math.min(100, Math.max(0, ((value - min) / (end - min)) * 100))
    : 0
  const shown = (n: number) => n / scale
  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between gap-3">
        <label htmlFor={id} className="text-sm font-bold">
          {label}
        </label>
        <div className="border-line bg-canvas flex shrink-0 items-center gap-1 rounded-lg border px-2 py-1.5 focus-within:border-accent">
          <input
            id={id}
            type="number"
            value={finite ? shown(value) : ''}
            min={min / scale}
            max={max / scale}
            step={step === 1 ? step / scale : 'any'}
            aria-invalid={!valid}
            aria-describedby={`${id}-unit${hint ? ` ${id}-hint` : ''}`}
            onChange={(e) =>
              onChange(
                e.target.value === ''
                  ? NaN
                  : scale === 1
                    ? Number(e.target.value)
                    : Math.round(Number(e.target.value) * scale * 1e8) / 1e8
              )
            }
            className="w-20 bg-transparent text-right text-sm font-bold tabular-nums outline-none"
          />
          <span id={`${id}-unit`} className="text-muted text-xs">
            {unit}
          </span>
        </div>
      </div>
      {sliderMax !== undefined && (
        <>
          <input
            type="range"
            aria-label={`${label} slider`}
            aria-valuetext={valid ? `${shown(value)}${unit}` : 'Choose a value'}
            min={min}
            max={end}
            step={step}
            value={valid ? value : min}
            onChange={(e) => onChange(Number(e.target.value))}
            className="module-range w-full cursor-pointer"
            style={{
              background: `linear-gradient(to right, var(--tx-accent) ${fill}%, var(--tx-bg-active) ${fill}%)`,
            }}
          />
          <div className="text-muted flex justify-between text-xs">
            <span>
              {shown(min)}
              {unit}
            </span>
            <span>
              {shown(end)}
              {unit}
            </span>
          </div>
        </>
      )}
      {hint && (
        <p id={`${id}-hint`} className="text-muted text-xs leading-relaxed">
          {hint}
        </p>
      )}
    </div>
  )
}

export function AllocationBar({
  share,
  target,
  limit,
  currency,
}: {
  share: number
  target: number
  limit: number
  currency: string
}) {
  return (
    <div
      className="space-y-2"
      role="img"
      aria-label={`${currency} holdings ${percentage(share)}; target ${percentage(target)}; purchase limit ${percentage(limit)}`}
    >
      <div className="bg-active relative h-3 rounded-full">
        <div
          className="h-full rounded-full bg-accent transition-[width] motion-reduce:transition-none"
          style={{ width: `${Math.min(100, Math.max(0, share / 100))}%` }}
        />
        <span
          className="border-ink absolute -top-1 h-5 border-l-2"
          style={{ left: `${target / 100}%` }}
        />
        <span
          className="border-muted absolute -top-1 h-5 border-l-2 border-dashed"
          style={{ left: `${limit / 100}%` }}
        />
      </div>
      <div className="text-muted flex flex-wrap justify-between gap-2 text-xs">
        <span>Target {percentage(target)}</span>
        <span>Purchase limit {percentage(limit)}</span>
      </div>
    </div>
  )
}

export function MarginBars({
  buy,
  sell,
  extra = false,
}: {
  buy: number | null
  sell: number | null
  extra?: boolean
}) {
  const max = Math.max(1, buy ?? 0, sell ?? 0)
  return (
    <div className="space-y-4" aria-live="polite">
      {(
        [
          ['Vault buys', buy],
          ['Vault sells', sell],
        ] as const
      ).map(([label, value], i) => (
        <div key={label}>
          <div className="mb-2 flex justify-between gap-3 text-sm">
            <span>{label}</span>
            <strong className="tabular-nums">
              {value === null
                ? 'Paused'
                : `${extra ? '+' : ''}${percentage(value)}`}
            </strong>
          </div>
          <div className="bg-active h-2 rounded-full">
            <div
              className={`h-full rounded-full transition-[width] motion-reduce:transition-none ${i === 0 ? 'bg-accent' : 'bg-muted'}`}
              style={{ width: `${((value ?? 0) / max) * 100}%` }}
            />
          </div>
        </div>
      ))}
    </div>
  )
}

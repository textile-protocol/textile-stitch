// Small shared primitives, styled off the --tx-* tokens. Kept in one file because
// each is a handful of lines and splitting them would be more ceremony than the
// panel needs.

import type { ReactNode } from 'react'
import type { BotState, WarningBody } from '../types'

export function Card({
  title,
  action,
  children,
  className = '',
}: {
  title?: ReactNode
  action?: ReactNode
  children: ReactNode
  className?: string
}) {
  return (
    <section
      className={`rounded-xl border border-line-soft bg-surface p-5 ${className}`}
    >
      {(title || action) && (
        <header className="mb-4 flex items-start justify-between gap-4">
          {typeof title === 'string' ? (
            <h2 className="text-base font-bold">{title}</h2>
          ) : (
            title
          )}
          {action}
        </header>
      )}
      {children}
    </section>
  )
}

type ButtonVariant = 'primary' | 'secondary' | 'danger' | 'ghost'

const BUTTON_STYLES: Record<ButtonVariant, string> = {
  primary: 'bg-accent text-on-accent hover:opacity-90',
  secondary: 'border border-line bg-surface hover:bg-hover',
  danger: 'border border-line bg-danger-bg text-danger hover:opacity-90',
  ghost: 'text-muted hover:bg-hover',
}

export function Button({
  variant = 'secondary',
  busy = false,
  children,
  className = '',
  ...rest
}: {
  variant?: ButtonVariant
  busy?: boolean
} & React.ButtonHTMLAttributes<HTMLButtonElement>) {
  return (
    <button
      {...rest}
      disabled={rest.disabled || busy}
      className={`inline-flex items-center gap-2 rounded-lg px-3 py-1.5 text-sm font-bold transition disabled:cursor-not-allowed disabled:opacity-45 ${BUTTON_STYLES[variant]} ${className}`}
    >
      {busy && <Spinner />}
      {children}
    </button>
  )
}

export function Spinner() {
  return (
    <span
      aria-hidden
      className="inline-block size-3.5 animate-spin rounded-full border-2 border-current border-t-transparent"
    />
  )
}

/** Container state as a pill. Only `running` is green: see docker::ContainerState. */
export function StatePill({ state, status }: { state: BotState; status: string }) {
  const tone =
    state === 'running'
      ? 'bg-success-bg text-success'
      : state === 'dead' || state === 'exited'
        ? 'bg-danger-bg text-danger'
        : state === 'restarting' || state === 'created'
          ? 'bg-warning-bg text-warning'
          : 'bg-hover text-muted'
  return (
    <span
      title={status}
      className={`inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-bold ${tone}`}
    >
      {state}
    </span>
  )
}

export function Tag({ children }: { children: ReactNode }) {
  return (
    <span className="rounded-md border border-line-soft px-1.5 py-0.5 text-xs text-muted">
      {children}
    </span>
  )
}

/** A bot's warnings. Blocking ones read as errors, the rest as advisories. */
export function Warnings({ warnings }: { warnings: WarningBody[] }) {
  if (warnings.length === 0) return null
  return (
    <ul className="space-y-2">
      {warnings.map((w) => (
        <li
          key={w.kind + w.message}
          className={`rounded-lg px-3 py-2 text-sm ${
            w.blocksEditing
              ? 'bg-danger-bg text-danger'
              : 'bg-warning-bg text-warning'
          }`}
        >
          {w.message}
        </li>
      ))}
    </ul>
  )
}

export function Field({
  label,
  hint,
  children,
}: {
  label: string
  hint?: ReactNode
  children: ReactNode
}) {
  return (
    <label className="block">
      <span className="mb-1 block text-sm font-bold">{label}</span>
      {children}
      {hint && <span className="mt-1 block text-xs text-faint">{hint}</span>}
    </label>
  )
}

const INPUT_CLASS =
  'w-full rounded-lg border border-line bg-canvas px-3 py-1.5 text-sm text-ink placeholder:text-faint disabled:opacity-60'

export function Input(props: React.InputHTMLAttributes<HTMLInputElement>) {
  return <input {...props} className={`${INPUT_CLASS} ${props.className ?? ''}`} />
}

export function Select(props: React.SelectHTMLAttributes<HTMLSelectElement>) {
  return <select {...props} className={`${INPUT_CLASS} ${props.className ?? ''}`} />
}

export function TextArea(props: React.TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return (
    <textarea
      {...props}
      className={`${INPUT_CLASS} font-mono text-xs leading-relaxed ${props.className ?? ''}`}
    />
  )
}

export function Toggle({
  checked,
  onChange,
  label,
  disabled,
}: {
  checked: boolean
  onChange: (v: boolean) => void
  label: string
  disabled?: boolean
}) {
  return (
    <label className="flex cursor-pointer items-center gap-2 text-sm">
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
        className="size-4 accent-[var(--tx-accent)]"
      />
      <span>{label}</span>
    </label>
  )
}

export function Banner({
  tone,
  children,
  onDismiss,
}: {
  tone: 'info' | 'success' | 'warning' | 'danger'
  children: ReactNode
  onDismiss?: () => void
}) {
  const styles = {
    info: 'bg-hover text-ink',
    success: 'bg-success-bg text-success',
    warning: 'bg-warning-bg text-warning',
    danger: 'bg-danger-bg text-danger',
  }[tone]
  return (
    <div className={`flex items-start gap-3 rounded-lg px-3 py-2 text-sm ${styles}`}>
      <div className="flex-1">{children}</div>
      {onDismiss && (
        <button
          onClick={onDismiss}
          aria-label="Dismiss"
          className="shrink-0 opacity-60 hover:opacity-100"
        >
          ✕
        </button>
      )}
    </div>
  )
}

/** Loading, error and empty states, so no caller has to invent its own. */
export function Loading({ what }: { what: string }) {
  return (
    <div className="flex items-center gap-2 py-8 text-sm text-muted">
      <Spinner /> Loading {what}…
    </div>
  )
}

export function ErrorState({
  error,
  onRetry,
}: {
  error: string
  onRetry?: () => void
}) {
  return (
    <div className="space-y-3 py-6">
      <Banner tone="danger">{error}</Banner>
      {onRetry && (
        <Button onClick={onRetry} variant="secondary">
          Try again
        </Button>
      )}
    </div>
  )
}

export function Empty({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="rounded-xl border border-dashed border-line py-12 text-center">
      <p className="font-bold">{title}</p>
      {children && <div className="mt-2 text-sm text-muted">{children}</div>}
    </div>
  )
}

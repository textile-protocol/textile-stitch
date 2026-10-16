import { useEffect, useRef, useState, type ReactNode } from 'react'

/**
 * A how-to that opens over the page instead of navigating away, so a
 * half-filled form (and the secret in it) stays exactly where it was.
 *
 * A native modal `<dialog>`: the browser traps focus, Escape closes it, and
 * focus goes back to whatever opened it. The close button and a click on the
 * dimmed backdrop go through `close()` too, so every way out reaches `onClose`
 * the same way. Rendered only while open, so the guide's video doesn't load until
 * someone asks for it.
 */
export function GuideSheet({
  title,
  open,
  onClose,
  children,
}: {
  title: string
  open: boolean
  onClose: () => void
  children: ReactNode
}) {
  const ref = useRef<HTMLDialogElement>(null)

  useEffect(() => {
    const dialog = ref.current
    if (open && dialog && !dialog.open) dialog.showModal()
  }, [open])

  if (!open) return null
  return (
    <dialog
      ref={ref}
      aria-label={title}
      onClose={onClose}
      onClick={(e) => {
        // The dialog element itself only receives the click on its backdrop.
        if (e.target === e.currentTarget) ref.current?.close()
      }}
      className="border-line bg-surface text-ink m-0 ml-auto h-dvh max-h-dvh w-full max-w-2xl border-l p-0 backdrop:bg-black/50"
    >
      <div className="flex h-full flex-col">
        <header className="border-line-soft flex items-center justify-between gap-4 border-b px-5 py-4">
          <h2 className="text-base font-bold">{title}</h2>
          <button
            type="button"
            onClick={() => ref.current?.close()}
            aria-label="Close guide"
            className="text-muted hover:bg-hover rounded-lg px-2 py-1"
          >
            ✕
          </button>
        </header>
        <div className="flex-1 overflow-y-auto px-5 py-5">{children}</div>
      </div>
    </dialog>
  )
}

/**
 * "No key yet? <label>" with the guide behind it. Owns the open state, so a
 * signer form adds a guide with one element.
 */
export function GuideLink({
  label,
  title,
  children,
}: {
  label: string
  title: string
  children: ReactNode
}) {
  const [open, setOpen] = useState(false)
  return (
    <>
      <p className="text-muted text-sm">
        No key yet?{' '}
        <button
          type="button"
          onClick={() => setOpen(true)}
          className="font-bold text-accent underline-offset-2 hover:underline"
        >
          {label}
        </button>
      </p>
      <GuideSheet title={title} open={open} onClose={() => setOpen(false)}>
        {children}
      </GuideSheet>
    </>
  )
}

/** A numbered step: what to do, then whatever the step needs to show for it. */
export function GuideStep({
  n,
  title,
  children,
}: {
  n: number
  title: string
  children: ReactNode
}) {
  return (
    <section className="flex gap-3">
      <span
        aria-hidden
        className="text-on-accent flex size-6 shrink-0 items-center justify-center rounded-full bg-accent text-xs font-bold"
      >
        {n}
      </span>
      <div className="min-w-0 flex-1 space-y-2 text-sm">
        <h3 className="font-bold">{title}</h3>
        {children}
      </div>
    </section>
  )
}

/**
 * Something to paste somewhere else, with a Copy button. The panel is often
 * reached over plain http on a LAN address, where the Clipboard API doesn't
 * exist, so there Copy selects the text for a manual copy instead.
 */
export function CopyBlock({ text, label }: { text: string; label: string }) {
  const pre = useRef<HTMLPreElement>(null)
  const [copied, setCopied] = useState(false)

  async function copy() {
    try {
      await navigator.clipboard.writeText(text)
      setCopied(true)
      setTimeout(() => setCopied(false), 1500)
    } catch {
      const range = document.createRange()
      if (!pre.current) return
      range.selectNodeContents(pre.current)
      const selection = window.getSelection()
      selection?.removeAllRanges()
      selection?.addRange(range)
    }
  }

  return (
    <div className="relative">
      <pre
        ref={pre}
        aria-label={label}
        className="border-line-soft bg-canvas overflow-x-auto rounded-lg border p-3 pr-16 font-mono text-xs leading-relaxed"
      >
        {text}
      </pre>
      <button
        type="button"
        onClick={() => void copy()}
        className="border-line bg-surface hover:bg-hover absolute right-2 top-2 rounded-md border px-2 py-0.5 text-xs font-bold"
      >
        {copied ? 'Copied' : 'Copy'}
      </button>
    </div>
  )
}

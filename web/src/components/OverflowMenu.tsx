import { useEffect, useId, useRef, useState } from 'react'
import { Link, useLocation } from 'react-router-dom'
import { api } from '../api'
import type { SessionInfo } from '../types'
import ComposeExportLink from './ComposeExportLink'

const ITEM =
  'block w-full px-3 py-2 text-left text-sm text-ink hover:bg-hover disabled:opacity-60'

/**
 * Secondary header actions behind a single control so the bar stays usable on
 * a phone. Fleet stays in the header itself — this is only for the rest.
 *
 * A disclosure, not an ARIA menu: the panel mixes a non-action identity label
 * with buttons and a link, so promising menu keyboard semantics would be a lie.
 * Escape closes and returns focus to the trigger.
 */
export default function OverflowMenu({
  session,
  onSignedOut,
}: {
  session: SessionInfo
  onSignedOut: () => void
}) {
  const [open, setOpen] = useState(false)
  const rootRef = useRef<HTMLDivElement>(null)
  const triggerRef = useRef<HTMLButtonElement>(null)
  const panelId = useId()
  const { pathname } = useLocation()

  useEffect(() => {
    setOpen(false)
  }, [pathname])

  useEffect(() => {
    if (!open) return
    function onPointer(e: MouseEvent | TouchEvent) {
      if (!rootRef.current?.contains(e.target as Node)) setOpen(false)
    }
    function onKey(e: KeyboardEvent) {
      if (e.key !== 'Escape') return
      setOpen(false)
      // Return focus so Escape doesn't drop the keyboard user into nowhere.
      triggerRef.current?.focus()
    }
    document.addEventListener('mousedown', onPointer)
    document.addEventListener('touchstart', onPointer)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onPointer)
      document.removeEventListener('touchstart', onPointer)
      document.removeEventListener('keydown', onKey)
    }
  }, [open])

  return (
    <div className="relative" ref={rootRef}>
      <button
        ref={triggerRef}
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="inline-flex size-9 items-center justify-center rounded-lg text-muted hover:bg-hover hover:text-ink"
        aria-label={open ? 'Close menu' : 'Open menu'}
        aria-expanded={open}
        aria-controls={panelId}
      >
        <MenuIcon />
      </button>
      {open && (
        <div
          id={panelId}
          className="absolute right-0 top-full z-50 mt-1 w-56 max-w-[calc(100vw-2rem)] overflow-hidden rounded-xl border border-line-soft bg-surface py-1 shadow-lg"
        >
          {session.identity && (
            <>
              <div
                className="truncate px-3 py-2 text-xs text-faint"
                title={session.identity}
              >
                {session.identity}
              </div>
              <div className="my-1 border-t border-line-soft" />
            </>
          )}
          <ComposeExportLink
            className={ITEM}
            title="A generated compose file for the whole fleet, for disaster recovery"
          >
            Export compose
          </ComposeExportLink>
          <Link to="/add" className={ITEM} onClick={() => setOpen(false)}>
            Add a bot
          </Link>
          {session.passwordLogin && (
            <>
              <div className="my-1 border-t border-line-soft" />
              <button
                type="button"
                className={ITEM}
                onClick={async () => {
                  setOpen(false)
                  await api.logout()
                  onSignedOut()
                }}
              >
                Sign out
              </button>
            </>
          )}
        </div>
      )}
    </div>
  )
}

function MenuIcon() {
  return (
    <svg
      width="18"
      height="18"
      viewBox="0 0 18 18"
      fill="none"
      aria-hidden
      className="block"
    >
      <path
        d="M3 4.5h12M3 9h12M3 13.5h12"
        stroke="currentColor"
        strokeWidth="1.75"
        strokeLinecap="round"
      />
    </svg>
  )
}

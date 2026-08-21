import { useEffect, useId, useRef, useState } from 'react'
import { Link, useLocation } from 'react-router-dom'
import { api } from '../api'
import { botPath, parseBotTab, type BotTab } from '../botRoutes'
import type { Bot } from '../types'
import { StatePill } from './ui'

/**
 * The detail-page title. When the fleet has another bot, the name is a
 * dropdown so you can jump there without going back to Fleet. The current
 * `?tab=` is kept so Logs stays Logs.
 */
export default function BotSwitcher({ name }: { name: string }) {
  const { search } = useLocation()
  const tab: BotTab = parseBotTab(new URLSearchParams(search).get('tab'))
  const [bots, setBots] = useState<Bot[] | null>(null)
  const [open, setOpen] = useState(false)
  const rootRef = useRef<HTMLDivElement>(null)
  const triggerRef = useRef<HTMLButtonElement>(null)
  const panelId = useId()

  useEffect(() => {
    let cancelled = false
    void api
      .fleet()
      .then((fleet) => {
        if (!cancelled) {
          setBots([...fleet.bots].sort((a, b) => a.name.localeCompare(b.name)))
        }
      })
      .catch(() => {
        if (!cancelled) setBots([])
      })
    return () => {
      cancelled = true
    }
  }, [name])

  useEffect(() => {
    setOpen(false)
  }, [name])

  useEffect(() => {
    if (!open) return
    function onPointer(e: MouseEvent | TouchEvent) {
      if (!rootRef.current?.contains(e.target as Node)) setOpen(false)
    }
    function onKey(e: KeyboardEvent) {
      if (e.key !== 'Escape') return
      setOpen(false)
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

  const others = bots?.filter((b) => b.name !== name) ?? []
  // No other bot to open: plain title, no button, no chevron.
  // `!bots` is for the type checker — `others.length === 0` already covers null.
  if (!bots || others.length === 0) {
    return <h1 className="text-xl font-bold">{name}</h1>
  }

  return (
    <div className="relative min-w-0" ref={rootRef}>
      <h1 className="min-w-0 text-xl font-bold">
        <button
          ref={triggerRef}
          type="button"
          onClick={() => setOpen((v) => !v)}
          className={`inline-flex max-w-full items-center gap-1.5 rounded-lg px-2 py-0.5 -mx-2 transition hover:bg-hover ${
            open ? 'bg-hover' : ''
          }`}
          aria-label={`Switch bot, current: ${name}`}
          aria-expanded={open}
          aria-controls={panelId}
          title="Switch bot"
        >
          <span className="truncate">{name}</span>
          <Chevron open={open} />
        </button>
      </h1>
      {open && (
        <div
          id={panelId}
          className="absolute left-0 top-full z-50 mt-1 max-h-80 w-72 max-w-[calc(100vw-2rem)] overflow-auto rounded-xl border border-line-soft bg-surface py-1 shadow-lg"
        >
          {bots.map((bot) => {
            const active = bot.name === name
            return (
              <Link
                key={bot.name}
                to={botPath(bot.name, tab)}
                onClick={() => setOpen(false)}
                className={`flex items-center gap-2 px-3 py-2 text-sm font-normal hover:bg-hover ${
                  active ? 'bg-accent-tint font-bold text-accent' : 'text-ink'
                }`}
                aria-current={active ? 'page' : undefined}
              >
                <span className="min-w-0 flex-1 truncate">{bot.name}</span>
                {bot.config?.corridorLabel && (
                  <span className="max-w-24 truncate text-xs font-normal text-muted">
                    {bot.config.corridorLabel}
                  </span>
                )}
                <StatePill state={bot.state} status={bot.status} />
              </Link>
            )
          })}
        </div>
      )}
    </div>
  )
}

function Chevron({ open }: { open: boolean }) {
  return (
    <svg
      width="14"
      height="14"
      viewBox="0 0 12 12"
      fill="none"
      aria-hidden
      className={`shrink-0 text-muted transition ${open ? 'rotate-180' : ''}`}
    >
      <path
        d="M2.5 4.5 6 8l3.5-3.5"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  )
}

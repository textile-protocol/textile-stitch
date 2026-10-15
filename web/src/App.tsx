import { useCallback, useEffect, useState } from 'react'
import { Link, Navigate, Route, Routes, useLocation, useParams } from 'react-router-dom'
import { ApiError, api, setUnauthorizedHandler } from './api'
import OverflowMenu from './components/OverflowMenu'
import PanelUpdateStatus, { usePanelUpdate } from './components/PanelUpdateStatus'
import TextileIcon from './components/TextileIcon'
import { Button, Loading } from './components/ui'
import Login from './pages/Login'
import Fleet from './pages/Fleet'
import AddBot from './pages/AddBot'
import BotDetail from './pages/BotDetail'
import { shortImage } from './format'
import { panelUpdateBusy } from './panelUpdate'
import type { SessionInfo, UpdatesStatus } from './types'

/** Persisted so the operator's choice survives a reload. */
const THEME_KEY = 'stitch-panel-theme'

/**
 * How often the header re-queries the registry for a newer panel image.
 * Soft checks on navigation reuse the server's 15-minute cache; this interval
 * forces a fresh lookup so a long-lived tab still notices an update.
 */
const UPDATES_POLL_MS = 5 * 60 * 1000

export default function App() {
  const [session, setSession] = useState<SessionInfo | null>(null)
  const [error, setError] = useState<string | null>(null)
  // Owned here, not in the header, so the login screen is themed too and there is
  // only ever one source of truth for `data-theme`.
  const [theme, setTheme] = useTheme()

  const refresh = useCallback(async () => {
    try {
      setSession(await api.session())
      setError(null)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    }
  }, [])

  useEffect(() => {
    void refresh()
  }, [refresh])

  // A password session expires after 12 hours, and the first thing the operator
  // knows about it is a button that fails. Re-asking who we are turns that into
  // the login screen instead of a page where nothing works.
  useEffect(() => {
    setUnauthorizedHandler(() => void refresh())
    return () => setUnauthorizedHandler(null)
  }, [refresh])

  if (error && !session) {
    return (
      <Centered>
        <p className="text-sm text-danger">{error}</p>
        <Button onClick={() => void refresh()}>Try again</Button>
      </Centered>
    )
  }
  if (!session) {
    return (
      <Centered>
        <Loading what="Stitch" />
      </Centered>
    )
  }
  if (!session.authenticated) {
    return <Login session={session} onSignedIn={() => void refresh()} />
  }

  return (
    <div className="min-h-screen">
      <Header
        session={session}
        theme={theme}
        onTheme={setTheme}
        onSignedOut={() => void refresh()}
      />
      <main className="mx-auto max-w-5xl px-6 py-8">
        <Routes>
          <Route path="/" element={<Fleet />} />
          <Route path="/add" element={<AddBot rfqDefault={session.rfqDefault} />} />
          <Route path="/bots/:name" element={<BotDetailPage />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </main>
      <footer className="mx-auto max-w-5xl px-6 pb-8 text-xs text-faint">
        Stitch v{session.version} · <span className="font-mono">{session.configDir}</span>
      </footer>
    </div>
  )
}

/** Fresh instance per bot so settings, tools, and banners don't leak across names. */
function BotDetailPage() {
  const { name } = useParams()
  return <BotDetail key={name} />
}

function Header({
  session,
  theme,
  onTheme,
  onSignedOut,
}: {
  session: SessionInfo
  theme: Theme
  onTheme: (t: Theme) => void
  onSignedOut: () => void
}) {
  const { pathname } = useLocation()
  const [updates, setUpdates] = useState<UpdatesStatus | null>(null)
  const panelUpdate = usePanelUpdate()
  const panelBusy = panelUpdateBusy(panelUpdate.state)

  // Soft check on every navigation (and mount); forced registry refresh on a
  // timer so a parked tab still learns about a newer panel image.
  useEffect(() => {
    let cancelled = false
    async function check(force: boolean) {
      try {
        const u = await api.updates(force)
        if (!cancelled) setUpdates(u)
      } catch {
        if (!cancelled) setUpdates(null)
      }
    }
    void check(false)
    const timer = setInterval(() => void check(true), UPDATES_POLL_MS)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [pathname])

  return (
    <header className="border-b border-line-soft bg-surface">
      <div className="mx-auto flex max-w-5xl items-center gap-2 px-4 py-3 sm:gap-4 sm:px-6">
        <Link to="/" className="flex min-w-0 items-center gap-2 font-bold">
          <TextileIcon className="h-5 w-5 shrink-0" />
          <span className="truncate">Stitch</span>
        </Link>
        <nav className="flex shrink-0 items-center gap-1 text-sm">
          <NavLink to="/" current={pathname === '/'}>
            Fleet
          </NavLink>
        </nav>
        <div className="ml-auto flex shrink-0 items-center gap-1 sm:gap-2">
          {updates?.panel.updateAvailable && (
            <Button
              variant="primary"
              busy={panelBusy}
              onClick={() => void panelUpdate.start(updates.panel.targetImage)}
              title={`Update panel to ${shortImage(updates.panel.targetImage)}`}
              className="max-sm:px-2 max-sm:text-xs"
            >
              <span className="sm:hidden">{panelBusy ? 'Updating' : 'Update'}</span>
              <span className="hidden sm:inline">
                {panelBusy ? 'Updating panel' : 'Update panel'}
              </span>
            </Button>
          )}
          <button
            type="button"
            onClick={() => onTheme(theme === 'dark' ? 'light' : 'dark')}
            className="inline-flex size-9 items-center justify-center rounded-lg text-muted hover:bg-hover hover:text-ink"
            aria-label="Switch theme"
          >
            {theme === 'dark' ? '☾' : '☀'}
          </button>
          <OverflowMenu session={session} onSignedOut={onSignedOut} />
        </div>
      </div>
      <PanelUpdateStatus state={panelUpdate.state} onDismiss={panelUpdate.dismiss} />
    </header>
  )
}

function NavLink({
  to,
  current,
  children,
}: {
  to: string
  current: boolean
  children: React.ReactNode
}) {
  return (
    <Link
      to={to}
      className={`rounded-lg px-2.5 py-1 ${
        current ? 'bg-accent-tint font-bold text-accent' : 'text-muted hover:bg-hover'
      }`}
    >
      {children}
    </Link>
  )
}

function Centered({ children }: { children: React.ReactNode }) {
  return (
    <div className="grid min-h-screen place-items-center">
      <div className="flex flex-col items-center gap-4">{children}</div>
    </div>
  )
}

type Theme = 'light' | 'dark'

/** Theme state, applied to `<html data-theme>` where the CSS variables key off it. */
function useTheme(): [Theme, (t: Theme) => void] {
  const [theme, setTheme] = useState<Theme>(() => {
    const stored = localStorage.getItem(THEME_KEY)
    if (stored === 'light' || stored === 'dark') return stored
    return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'
  })

  useEffect(() => {
    document.documentElement.dataset.theme = theme
    localStorage.setItem(THEME_KEY, theme)
  }, [theme])

  return [theme, setTheme]
}

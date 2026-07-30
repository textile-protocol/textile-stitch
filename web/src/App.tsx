import { useCallback, useEffect, useState } from 'react'
import { Link, Navigate, Route, Routes, useLocation } from 'react-router-dom'
import { ApiError, api, setUnauthorizedHandler } from './api'
import ComposeExportLink from './components/ComposeExportLink'
import TextileIcon from './components/TextileIcon'
import { Button, Loading } from './components/ui'
import Login from './pages/Login'
import Fleet from './pages/Fleet'
import AddBot from './pages/AddBot'
import BotDetail from './pages/BotDetail'
import type { SessionInfo } from './types'

/** Persisted so the operator's choice survives a reload. */
const THEME_KEY = 'stitch-panel-theme'

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
        <Loading what="the panel" />
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
          <Route path="/add" element={<AddBot />} />
          <Route path="/bots/:name" element={<BotDetail />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </main>
    </div>
  )
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

  return (
    <header className="border-b border-line-soft bg-surface">
      <div className="mx-auto flex max-w-5xl items-center gap-4 px-6 py-3">
        <Link to="/" className="flex items-center gap-2 font-bold">
          <TextileIcon className="h-5 w-5 shrink-0" />
          {/*
            One span, so the icon and the wordmark are the only two flex items and
            `gap-2` applies between them and not between "Stitch" and "panel" — a
            bare text node beside the span would become its own flex item.
          */}
          <span>
            Stitch <span className="text-muted">panel</span>
          </span>
        </Link>
        <nav className="flex items-center gap-1 text-sm">
          <NavLink to="/" current={pathname === '/'}>
            Fleet
          </NavLink>
          <NavLink to="/add" current={pathname === '/add'}>
            Add a bot
          </NavLink>
        </nav>
        <div className="ml-auto flex items-center gap-3 text-sm">
          <ComposeExportLink
            className="text-muted underline decoration-line hover:text-ink disabled:opacity-60"
            title="A generated compose file for the whole fleet, for disaster recovery"
          >
            Export compose
          </ComposeExportLink>
          <button
            onClick={() => onTheme(theme === 'dark' ? 'light' : 'dark')}
            className="text-muted hover:text-ink"
            aria-label="Switch theme"
          >
            {theme === 'dark' ? '☾' : '☀'}
          </button>
          <span className="text-faint" title="Signed in as">
            {session.identity}
          </span>
          {session.passwordLogin && (
            <Button
              variant="ghost"
              onClick={async () => {
                await api.logout()
                onSignedOut()
              }}
            >
              Sign out
            </Button>
          )}
        </div>
      </div>
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

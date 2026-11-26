import { useCallback, useEffect, useState } from 'react'
import { Link, Navigate, Route, Routes, useLocation, useParams } from 'react-router-dom'
import { ApiError, api, setUnauthorizedHandler } from './api'
import OverflowMenu from './components/OverflowMenu'
import PanelUpdateStatus from './components/PanelUpdateStatus'
import PanelUpdateSheet, { type BotsSection } from './components/PanelUpdateSheet'
import TextileIcon from './components/TextileIcon'
import PanelPasswordSetup from './components/PanelPasswordSetup'
import { Banner, Button, Loading } from './components/ui'
import Login from './pages/Login'
import Fleet from './pages/Fleet'
import AddBot from './pages/AddBot'
import BotDetail from './pages/BotDetail'
import FleetChanges from './pages/FleetChanges'
import { arrangeFleet, readFleetOrder } from './fleetOrder'
import { planFleetUpdate, runActive } from './fleetUpdate'
import { afterPanelView, panelUpdateBusy, panelUpdateView } from './panelUpdate'
import { afterPanel, panelUpdate, useAfterPanel, usePanelUpdate } from './usePanelUpdate'
import { fleetUpdate, useFleetUpdate } from './useFleetUpdate'
import { ProfileContext, allows, profileOf } from './profile'
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
  const [passwordSaved, setPasswordSaved] = useState(false)
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

  const profile = profileOf(session)
  return (
    <ProfileContext.Provider value={profile}>
    <div className="min-h-screen">
      <Header
        session={session}
        theme={theme}
        onTheme={setTheme}
        onSignedOut={() => void refresh()}
      />
      <main className="mx-auto max-w-[96rem] px-6 py-8">
        {session.passwordSetupRequired && (
          <PanelPasswordSetup onSecured={(secured) => {
            setSession(secured)
            setPasswordSaved(true)
          }} />
        )}
        {passwordSaved && (
          <div className="mb-6" role="status">
            <Banner tone="success" onDismiss={() => setPasswordSaved(false)}>
              Panel password saved. Password sign-in is now required for all operators.
              Your bots are still running.
            </Banner>
          </div>
        )}
        <Routes>
          <Route path="/" element={<Fleet />} />
          <Route path="/changes" element={<FleetChanges />} />
          <Route
            path="/add"
            element={
              allows(profile, 'addBot') ? (
                <AddBot rfqDefault={session.rfqDefault} />
              ) : (
                <Navigate to="/" replace />
              )
            }
          />
          <Route path="/bots/:name" element={<BotDetailPage />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </main>
      <footer className="mx-auto max-w-[96rem] px-6 pb-8 text-xs text-faint">
        Stitch v{session.version} · <span className="font-mono">{session.configDir}</span>
        {profile === 'hosted' && ' · hosted'}
      </footer>
    </div>
    </ProfileContext.Provider>
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
  const panel = usePanelUpdate()
  const after = useAfterPanel()
  const run = useFleetUpdate()
  const panelBusy = panelUpdateBusy(panel)
  const botsUpdating = runActive(run)
  const profile = profileOf(session)
  // Hosted panels are updated by the host's Terraform, by digest.
  const canSelfUpdate = allows(profile, 'panelUpdate')
  const canUpdateBots = allows(profile, 'updateAll')
  const [sheetOpen, setSheetOpen] = useState(false)
  const [sheetBots, setSheetBots] = useState<BotsSection | null>(null)

  // This page may be the one a panel update reloaded onto, with bots handed
  // over to update now. Only the first call in a page load acts.
  useEffect(() => {
    afterPanel.resume(session.version, canUpdateBots)
  }, [session.version, canUpdateBots])

  async function openSheet() {
    setSheetOpen(true)
    if (!canUpdateBots) {
      setSheetBots(null)
      return
    }
    setSheetBots({ state: 'loading' })
    try {
      const [fleet, u] = await Promise.all([api.fleet(), api.updates()])
      setUpdates(u)
      setSheetBots({
        state: 'ready',
        // Fleet order, so the run goes down the list the operator knows.
        plan: planFleetUpdate(arrangeFleet(fleet.bots, readFleetOrder()), u),
      })
    } catch (e) {
      setSheetBots({ state: 'error', error: e instanceof ApiError ? e.message : String(e) })
    }
  }

  function dismissStrip() {
    if (panel.phase === 'failed') {
      panelUpdate.dismiss()
      return
    }
    // The strip and the Fleet card are one outcome; clearing one clears both.
    if (after.phase === 'bots' && !botsUpdating) fleetUpdate.dismiss()
    afterPanel.dismiss()
  }

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
    if (!canSelfUpdate) return
    void check(false)
    const timer = setInterval(() => void check(true), UPDATES_POLL_MS)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [pathname, canSelfUpdate])

  return (
    <header className="border-b border-line-soft bg-surface">
      <div className="mx-auto flex max-w-[96rem] items-center gap-2 px-4 py-3 sm:gap-4 sm:px-6">
        <Link to="/" className="flex min-w-0 items-center gap-2 font-bold">
          <TextileIcon className="h-5 w-5 shrink-0" />
          <span className="truncate">Stitch</span>
        </Link>
        <nav className="flex shrink-0 items-center gap-1 text-sm">
          <NavLink to="/" current={pathname === '/' || pathname === '/changes'}>
            Fleet
          </NavLink>
        </nav>
        <div className="ml-auto flex shrink-0 items-center gap-1 sm:gap-2">
          {canSelfUpdate && updates?.panel.updateAvailable && (
            <Button
              variant="primary"
              busy={panelBusy}
              // Restarting the panel mid-run would cut a bot's swap short.
              disabled={botsUpdating}
              onClick={() => void openSheet()}
              title={
                botsUpdating
                  ? 'Your bots are updating. You can update Fleet once they finish.'
                  : 'Update Stitch Fleet and your bots'
              }
              className="max-sm:px-2 max-sm:text-xs"
            >
              {panelBusy ? 'Updating' : 'Update'}
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
      <PanelUpdateStatus
        view={panelUpdateView(panel) ?? afterPanelView(after, run, pathname === '/')}
        showFleetLink={pathname !== '/'}
        onDismiss={dismissStrip}
      />
      {updates && (
        <PanelUpdateSheet
          open={sheetOpen}
          panelTarget={updates.panel.targetImage}
          version={session.version}
          bots={sheetBots}
          signsOut={session.identity === 'password'}
          onClose={() => setSheetOpen(false)}
          onStart={(bots) => {
            // The button is disabled mid-run too; this covers a run that
            // started while the sheet was open.
            if (botsUpdating) return
            panelUpdate.start({ bots, from: session.version })
          }}
        />
      )}
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

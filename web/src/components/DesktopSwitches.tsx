// "Keep Mac awake" and "Start at login", on the bot page.
//
// Both belong to the desktop app, not the panel; the panel relays them (see
// src/panel/desktop.rs). A click writes a request the app applies on its
// two-second tick, so the switch shows the asked-for value at once and the
// page re-reads until the app confirms. Renders nothing where there is no
// desktop app to talk to.

import { useEffect, useRef, useState } from 'react'
import { api, type DesktopSwitches as Switches } from '../api'
import { Toggle } from './ui'

const CONFIRM_POLL_MS = 1000
const CONFIRM_GIVE_UP_MS = 15_000

export default function DesktopSwitches() {
  const [state, setState] = useState<Switches | null>(null)
  const [error, setError] = useState<string | null>(null)
  const mountedRef = useRef(true)

  useEffect(() => {
    mountedRef.current = true
    void api
      .desktop()
      .then((s) => {
        if (mountedRef.current) setState(s)
      })
      .catch(() => {})
    return () => {
      mountedRef.current = false
    }
  }, [])

  // While a request is pending, ask again every second until the app has
  // applied it (pending clears) or long enough has passed to say so, and
  // then stop asking. Keyed on whether something is pending, not on the
  // pending object: that is a fresh value on every answer, and re-arming on
  // it reset the clock every second so the give-up never came.
  const pending = !!state?.pending
  useEffect(() => {
    if (!pending) return
    const startedAt = Date.now()
    const timer = window.setInterval(() => {
      if (Date.now() - startedAt > CONFIRM_GIVE_UP_MS) {
        clearInterval(timer)
        if (mountedRef.current) setError('The desktop app has not picked this up. Is it running?')
        return
      }
      void api
        .desktop()
        .then((s) => {
          if (!mountedRef.current) return
          setState(s)
          if (!s.pending) setError(null)
        })
        .catch(() => {})
    }, CONFIRM_POLL_MS)
    return () => clearInterval(timer)
  }, [pending])

  if (!state?.available) return null

  async function flip(which: 'autostart' | 'keepAwake', value: boolean) {
    setError(null)
    try {
      const next = await api.setDesktop({ [which]: value })
      if (mountedRef.current) setState(next)
    } catch (e) {
      if (mountedRef.current) setError(e instanceof Error ? e.message : String(e))
    }
  }

  return (
    <div className="flex flex-wrap items-center gap-x-5 gap-y-2">
      <Toggle
        checked={state.keepAwake}
        onChange={(v) => void flip('keepAwake', v)}
        label={state.keepAwakeLabel || 'Keep awake'}
      />
      <Toggle
        checked={state.autostart}
        onChange={(v) => void flip('autostart', v)}
        label="Start at login"
      />
      {state.pending && !error && <span className="text-xs text-faint">Applying…</span>}
      {error && <span className="text-xs text-warning">{error}</span>}
    </div>
  )
}

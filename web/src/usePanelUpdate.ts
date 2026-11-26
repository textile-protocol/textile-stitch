// The panel's self-update and the bots it takes along, kept outside any page.
//
// Two stores, one for each side of the restart:
// - The updater runs on the old panel. It arms the swap, leaves the bots the
//   operator ticked in a note (see panelHandoff.ts), waits for the panel to
//   drop out and come back, and reloads the page.
// - The handover runs on the new panel, once per page load, after sign-in. It
//   takes the note, asks the new panel what's behind, and hands the bots that
//   are still ready to the Update all runner.
// Both live at module level like the Update all run, so moving between pages
// doesn't lose them. Tests build their own with fakes.

import { useSyncExternalStore } from 'react'
import { ApiError, api } from './api'
import type { UpdateCandidate } from './fleetUpdate'
import {
  HANDOFF_KEY,
  decodeHandoff,
  encodeHandoff,
  planAfterPanel,
} from './panelHandoff'
import type { AfterPanel, PanelUpdate } from './panelUpdate'
import type { Bot, UpdatesStatus } from './types'
import { fleetUpdate } from './useFleetUpdate'

const POLL_MS = 2000
const POLL_ATTEMPTS = 60
/** How long a healthy answer counts as "not restarted yet" rather than "back". */
const GRACE_MS = 8_000
/**
 * The same, when bots are waiting on the new panel. Reloading onto the old
 * panel would make the handover refuse the bots, so wait longer for the drop
 * or the new version before trusting an answer that looks unchanged.
 */
const GRACE_WITH_BOTS_MS = 30_000

/** The slice of Storage the note needs. */
export interface KeyValueStore {
  getItem: (key: string) => string | null
  setItem: (key: string, value: string) => void
  removeItem: (key: string) => void
}

type Listener = () => void

function store<T>(initial: T) {
  let value = initial
  const listeners = new Set<Listener>()
  return {
    get: () => value,
    set(next: T) {
      value = next
      listeners.forEach((l) => l())
    },
    subscribe(listener: Listener) {
      listeners.add(listener)
      return () => {
        listeners.delete(listener)
      }
    },
  }
}

function message(e: unknown): string {
  return e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e)
}

export interface UpdaterDeps {
  /** POST /api/panel/update. Resolves once the swap is armed. */
  updatePanel: () => Promise<unknown>
  /** The version the panel answering now reports. Throws while it's down. */
  ping: () => Promise<string>
  reload: () => void
  /** Where the note goes. Null when the browser won't give us sessionStorage. */
  storage: KeyValueStore | null
  sleep: (ms: number) => Promise<void>
  now: () => number
  pollMs?: number
  pollAttempts?: number
  graceMs?: number
  graceWithBotsMs?: number
}

export interface UpdateRequest {
  /** Bots to update on the new panel, in run order. Empty for the panel alone. */
  bots: string[]
  /** This panel's version, so the new page can say what it moved from. */
  from: string
}

export interface PanelUpdater {
  /** Resolves when the page reloads or the update ends without one. */
  start: (req: UpdateRequest) => Promise<void>
  dismiss: () => void
  get: () => PanelUpdate
  subscribe: (listener: Listener) => () => void
}

export function createPanelUpdater(deps: UpdaterDeps): PanelUpdater {
  const state = store<PanelUpdate>({ phase: 'idle' })
  const pollMs = deps.pollMs ?? POLL_MS
  const attempts = deps.pollAttempts ?? POLL_ATTEMPTS

  function leaveNote(req: UpdateRequest): number {
    if (req.bots.length === 0) return 0
    try {
      deps.storage?.setItem(
        HANDOFF_KEY,
        encodeHandoff({ bots: req.bots, from: req.from, at: deps.now() }),
      )
      return deps.storage ? req.bots.length : 0
    } catch {
      // Quota or a locked-down browser. The panel still updates; the strip
      // stops promising bot updates it can't hand over.
      return 0
    }
  }

  /** Wait for the swap: the panel drops out, or answers with a new version. */
  async function awaitSwap(bots: number, from: string) {
    const grace = bots > 0 ? (deps.graceWithBotsMs ?? GRACE_WITH_BOTS_MS) : (deps.graceMs ?? GRACE_MS)
    const started = deps.now()
    let sawDown = false
    for (let i = 0; i < attempts; i++) {
      await deps.sleep(pollMs)
      try {
        const version = await deps.ping()
        // The helper sleeps ~2s then stop/rm/rename/start. Prefer seeing a
        // failed poll (the restart gap) or the new version, but a quick swap
        // can land between two polls on a release that kept its version, so
        // after a grace a healthy answer counts as back.
        const moved = sawDown || version !== from
        if (!moved && deps.now() - started < grace) continue
        // Soft API refreshes leave the browser on the old embedded SPA. The
        // new container serves a fresh index.html (no-cache) and hashed
        // assets, so reload to put version, shell and UI on the new image.
        state.set({ phase: 'reloading', bots })
        deps.reload()
        return
      } catch {
        sawDown = true
        state.set({ phase: 'restarting', bots })
      }
    }
    state.set({ phase: 'slow', bots })
  }

  return {
    async start(req) {
      const phase = state.get().phase
      if (phase !== 'idle' && phase !== 'failed') return
      const wanted = req.bots.length
      if (wanted > 0 && !deps.storage) {
        state.set({
          phase: 'failed',
          bots: 0,
          error:
            "This browser can't remember your bots while Fleet restarts. Turn off Stitch bots to update Fleet alone, then use Update all.",
        })
        return
      }
      state.set({ phase: 'pulling', bots: wanted })
      try {
        await deps.updatePanel()
      } catch (e) {
        state.set({ phase: 'failed', error: message(e), bots: wanted })
        return
      }
      const bots = leaveNote(req)
      state.set({ phase: 'armed', bots })
      await awaitSwap(bots, req.from)
    },
    dismiss() {
      if (state.get().phase === 'failed') state.set({ phase: 'idle' })
    },
    get: state.get,
    subscribe: state.subscribe,
  }
}

export interface HandoverDeps {
  storage: KeyValueStore | null
  now: () => number
  fleet: () => Promise<Bot[]>
  /** A fresh registry check, not the 15-minute cache. */
  updates: () => Promise<UpdatesStatus>
  startRun: (target: string, picked: UpdateCandidate[]) => void
}

export interface Handover {
  /**
   * Take the note and act on it. Only the first call in a page load does
   * anything. `allowed` is whether this panel lets the UI update bots in bulk;
   * when it doesn't, the note is dropped unread.
   */
  resume: (version: string, allowed: boolean) => Promise<void>
  /** How many bots a fresh note is waiting to update, without taking it. */
  pending: () => number
  dismiss: () => void
  get: () => AfterPanel
  subscribe: (listener: Listener) => () => void
}

export function createHandover(deps: HandoverDeps): Handover {
  const state = store<AfterPanel>({ phase: 'none' })
  let resumed = false

  function take(): string | null {
    try {
      const raw = deps.storage?.getItem(HANDOFF_KEY) ?? null
      // Out before anything acts on it: a reload mid-run mustn't start over.
      deps.storage?.removeItem(HANDOFF_KEY)
      return raw
    } catch {
      return null
    }
  }

  return {
    async resume(version, allowed) {
      if (resumed) return
      resumed = true
      const raw = take()
      if (!allowed) return
      const note = decodeHandoff(raw, deps.now())
      if (note.kind === 'none') return
      const { from, bots } = note.handoff
      if (note.kind === 'expired') {
        state.set({ phase: 'skipped', from, version, reason: 'expired' })
        return
      }
      state.set({ phase: 'checking', from, version, bots: bots.length })
      let plan
      try {
        const [fleet, updates] = await Promise.all([deps.fleet(), deps.updates()])
        plan = planAfterPanel(note.handoff, fleet, updates)
      } catch (e) {
        state.set({ phase: 'skipped', from, version, reason: 'check-failed', error: message(e) })
        return
      }
      if (plan.kind === 'skip') {
        state.set({ phase: 'skipped', from, version, reason: plan.reason })
        return
      }
      state.set({ phase: 'bots', from, version })
      deps.startRun(plan.target, plan.picked)
    },
    pending() {
      try {
        const note = decodeHandoff(deps.storage?.getItem(HANDOFF_KEY) ?? null, deps.now())
        return note.kind === 'ready' ? note.handoff.bots.length : 0
      } catch {
        return 0
      }
    },
    dismiss() {
      if (state.get().phase !== 'checking') state.set({ phase: 'none' })
    },
    get: state.get,
    subscribe: state.subscribe,
  }
}

/** sessionStorage, or null where the browser refuses it. */
function sessionStore(): KeyValueStore | null {
  try {
    const s = window.sessionStorage
    s.getItem(HANDOFF_KEY)
    return s
  } catch {
    return null
  }
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms))

const updater = createPanelUpdater({
  updatePanel: () => api.updatePanel(),
  ping: async () => (await api.session()).version,
  reload: () => window.location.reload(),
  storage: sessionStore(),
  sleep,
  now: () => Date.now(),
})

const handover = createHandover({
  storage: sessionStore(),
  now: () => Date.now(),
  fleet: async () => (await api.fleet()).bots,
  updates: () => api.updates(true),
  startRun: (target, picked) => fleetUpdate.start(target, picked),
})

export const panelUpdate = {
  start: (req: UpdateRequest) => void updater.start(req),
  dismiss: updater.dismiss,
}

export const afterPanel = {
  resume: (version: string, allowed: boolean) => void handover.resume(version, allowed),
  pending: handover.pending,
  dismiss: handover.dismiss,
}

export function usePanelUpdate(): PanelUpdate {
  return useSyncExternalStore(updater.subscribe, updater.get)
}

export function useAfterPanel(): AfterPanel {
  return useSyncExternalStore(handover.subscribe, handover.get)
}

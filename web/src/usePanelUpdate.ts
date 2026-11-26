// Stitch Fleet's update, kept outside any page: the bots first, then Fleet UI
// (the panel itself), then what happened. See panelUpdate.ts for why bots go
// first.
//
// Four stores:
// - The upgrade runs the ticked bots through the Update all runner and, once
//   every one came back, starts the Fleet UI update. If one didn't, or the
//   operator stopped the run, Fleet UI waits for them to say go.
// - The updater arms the Fleet UI restart, waits for it to drop out and come
//   back (or sign us out), and reloads the page onto the new version.
// - The outcome runs on the page that loads after, once per page load, and
//   says how it ended. It only displays; nothing acts on its note.
// - The handover is the older flow, Fleet UI first and bots after the reload,
//   which pages from v0.1.399 and v0.1.400 still start. It takes the bots they
//   leave behind and runs them, so an update started there finishes here.
// All live at module level like the Update all run, so moving between pages
// doesn't lose them. Tests build their own with fakes.

import { useSyncExternalStore } from 'react'
import { ApiError, api } from './api'
import { type FleetUpdateRun, type UpdateCandidate, runActive, runCounts } from './fleetUpdate'
import { HANDOFF_KEY, decodeHandoff, planAfterPanel } from './panelHandoff'
import {
  type AfterPanel,
  type FleetOutcome,
  type FleetUpgrade,
  type PanelUpdate,
  panelUpdateBusy,
} from './panelUpdate'
import type { Bot, UpdatesStatus } from './types'
import { OUTCOME_KEY, decodeOutcome, encodeOutcome } from './updateOutcome'
import { fleetUpdate } from './useFleetUpdate'

const POLL_MS = 2000
const POLL_ATTEMPTS = 60
/** How long a healthy answer counts as "not restarted yet" rather than "back". */
const GRACE_MS = 8_000

/** The slice of Storage the notes need. */
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

/** What the session route says about the Fleet UI answering now. */
export interface Ping {
  version: string
  authenticated: boolean
}

export interface UpdaterDeps {
  /** POST /api/panel/update. Resolves once the restart is armed. */
  updatePanel: () => Promise<unknown>
  /** The Fleet UI answering now. Throws while it's down. */
  ping: () => Promise<Ping>
  reload: () => void
  /** Where the outcome note goes. Null when the browser won't give us sessionStorage. */
  storage: KeyValueStore | null
  sleep: (ms: number) => Promise<void>
  now: () => number
  pollMs?: number
  pollAttempts?: number
  graceMs?: number
}

export interface UpdateRequest {
  /** This Fleet UI's version, so the next page can say what it moved from. */
  from: string
  /** Bots this flow updated first; 0 for Fleet UI alone. */
  bots: number
  /** Of the bots it tried, those that didn't come back up. */
  failed: number
  /** A password session, which the restart signs out. */
  signsOut: boolean
}

export interface PanelUpdater {
  /** Resolves when the page reloads or the update ends without one. */
  start: (req: UpdateRequest) => Promise<void>
  /** True from the download until the reload, while the old page should hold still. */
  inFlight: () => boolean
  dismiss: () => void
  get: () => PanelUpdate
  subscribe: (listener: Listener) => () => void
}

export function createPanelUpdater(deps: UpdaterDeps): PanelUpdater {
  const state = store<PanelUpdate>({ phase: 'idle' })
  const pollMs = deps.pollMs ?? POLL_MS
  const attempts = deps.pollAttempts ?? POLL_ATTEMPTS
  const grace = deps.graceMs ?? GRACE_MS

  function leaveNote(req: UpdateRequest) {
    try {
      deps.storage?.setItem(
        OUTCOME_KEY,
        encodeOutcome({ from: req.from, bots: req.bots, failed: req.failed, at: deps.now() }),
      )
    } catch {
      // Quota or a locked-down browser: the next page just won't say how it went.
    }
  }

  /**
   * Wait for the restart and reload onto the new version: as soon as Fleet UI
   * drops out and answers again, answers with a new version, or answers
   * without our session. That last one is what a password session sees:
   * sessions live in memory, so the new Fleet UI doesn't know ours. Reloading
   * right then means the sign-in form only ever appears on the new page, and
   * nothing reloads it while the operator types.
   */
  async function awaitRestart(req: UpdateRequest) {
    const step = { bots: req.bots, signsOut: req.signsOut }
    const started = deps.now()
    let sawDown = false
    for (let i = 0; i < attempts; i++) {
      await deps.sleep(pollMs)
      let ping: Ping
      try {
        ping = await deps.ping()
      } catch {
        sawDown = true
        state.set({ phase: 'restarting', ...step })
        continue
      }
      // The helper sleeps ~2s then stop/rm/rename/start. A quick restart can
      // land between two polls on a release that kept its version, so after
      // a grace a healthy answer counts as back too.
      const moved = sawDown || ping.version !== req.from || !ping.authenticated
      if (!moved && deps.now() - started < grace) continue
      // Soft API refreshes leave the browser on the old embedded SPA. The new
      // container serves a fresh index.html (no-cache) and hashed assets, so
      // reload to put version, shell and UI on the new build.
      state.set({ phase: 'reloading', ...step })
      deps.reload()
      return
    }
    state.set({ phase: 'slow', ...step })
  }

  return {
    async start(req) {
      const phase = state.get().phase
      if (phase !== 'idle' && phase !== 'failed') return
      const step = { bots: req.bots, signsOut: req.signsOut }
      state.set({ phase: 'pulling', ...step })
      try {
        await deps.updatePanel()
      } catch (e) {
        state.set({ phase: 'failed', error: message(e), ...step })
        return
      }
      leaveNote(req)
      state.set({ phase: 'armed', ...step })
      await awaitRestart(req)
    },
    inFlight: () => panelUpdateBusy(state.get()),
    dismiss() {
      if (state.get().phase === 'failed') state.set({ phase: 'idle' })
    },
    get: state.get,
    subscribe: state.subscribe,
  }
}

export interface UpgradeDeps {
  /** Run Update all on these bots and resolve with how it ended. */
  runBots: (target: string, picked: UpdateCandidate[]) => Promise<FleetUpdateRun | null>
  /** Start the Fleet UI update. */
  updateFleetUi: (req: UpdateRequest) => Promise<void>
}

export interface UpgradeRequest {
  /** The bot image the update check reported. */
  target: string
  /** Bots to update first, in run order. Empty for Fleet UI alone. */
  picked: UpdateCandidate[]
  from: string
  signsOut: boolean
}

export interface FleetUpgrader {
  start: (req: UpgradeRequest) => Promise<void>
  /** After a hold: update Fleet UI anyway. */
  resume: () => Promise<void>
  dismiss: () => void
  get: () => FleetUpgrade
  subscribe: (listener: Listener) => () => void
}

export function createFleetUpgrade(deps: UpgradeDeps): FleetUpgrader {
  const state = store<FleetUpgrade>({ phase: 'idle' })

  return {
    async start(req) {
      const phase = state.get().phase
      if (phase === 'bots') return
      const { from, signsOut } = req
      if (req.picked.length === 0) {
        state.set({ phase: 'idle' })
        await deps.updateFleetUi({ from, bots: 0, failed: 0, signsOut })
        return
      }
      state.set({ phase: 'bots', from, signsOut })
      const run = await deps.runBots(req.target, req.picked)
      // Null or still going means another run owned the runner; leave it be.
      if (!run || runActive(run)) {
        state.set({ phase: 'idle' })
        return
      }
      const c = runCounts(run)
      if (run.phase === 'done' && c.failed === 0) {
        state.set({ phase: 'idle' })
        await deps.updateFleetUi({ from, bots: c.done, failed: 0, signsOut })
        return
      }
      state.set({
        phase: 'held',
        from,
        signsOut,
        done: c.done,
        failed: c.failed,
        total: c.total,
        stopped: run.phase === 'cancelled',
      })
    },
    async resume() {
      const held = state.get()
      if (held.phase !== 'held') return
      state.set({ phase: 'idle' })
      await deps.updateFleetUi({
        from: held.from,
        bots: held.done,
        failed: held.failed,
        signsOut: held.signsOut,
      })
    },
    dismiss() {
      if (state.get().phase === 'held') state.set({ phase: 'idle' })
    },
    get: state.get,
    subscribe: state.subscribe,
  }
}

export interface OutcomeDeps {
  storage: KeyValueStore | null
  now: () => number
  /** A fresh update check, for a restart that kept its version string. */
  updates: () => Promise<UpdatesStatus>
}

export interface OutcomeReader {
  /** Take the note and show it. Only the first call in a page load does anything. */
  show: (version: string) => Promise<void>
  /** Whether a current note is waiting, without taking it. */
  pending: () => boolean
  dismiss: () => void
  get: () => FleetOutcome
  subscribe: (listener: Listener) => () => void
}

export function createOutcome(deps: OutcomeDeps): OutcomeReader {
  const state = store<FleetOutcome>({ phase: 'none' })
  let shown = false

  function read(take: boolean): string | null {
    try {
      const raw = deps.storage?.getItem(OUTCOME_KEY) ?? null
      if (take) deps.storage?.removeItem(OUTCOME_KEY)
      return raw
    } catch {
      return null
    }
  }

  return {
    async show(version) {
      if (shown) return
      shown = true
      const note = decodeOutcome(read(true), deps.now())
      if (!note) return
      if (note.from !== version) {
        state.set({ phase: 'updated', from: note.from, version, bots: note.bots, failed: note.failed })
        return
      }
      // Same version string: either a re-release under the same number, or the
      // restart didn't land. The update check tells them apart.
      try {
        const u = await deps.updates()
        state.set(
          u.panel.updateAvailable
            ? { phase: 'not-moved', bots: note.bots }
            : { phase: 'updated', from: note.from, version, bots: note.bots, failed: note.failed },
        )
      } catch {
        // Can't tell how it went, so say nothing rather than guess.
      }
    },
    pending: () => decodeOutcome(read(false), deps.now()) !== null,
    dismiss: () => state.set({ phase: 'none' }),
    get: state.get,
    subscribe: state.subscribe,
  }
}

/** The older flow; see the header. */
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
    s.getItem(OUTCOME_KEY)
    return s
  } catch {
    return null
  }
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms))

const updater = createPanelUpdater({
  updatePanel: () => api.updatePanel(),
  ping: async () => {
    const s = await api.session()
    return { version: s.version, authenticated: s.authenticated }
  },
  reload: () => window.location.reload(),
  storage: sessionStore(),
  sleep,
  now: () => Date.now(),
})

const upgrade = createFleetUpgrade({
  runBots: (target, picked) => fleetUpdate.run(target, picked),
  updateFleetUi: (req) => updater.start(req),
})

const outcome = createOutcome({
  storage: sessionStore(),
  now: () => Date.now(),
  updates: () => api.updates(true),
})

const handover = createHandover({
  storage: sessionStore(),
  now: () => Date.now(),
  fleet: async () => (await api.fleet()).bots,
  updates: () => api.updates(true),
  startRun: (target, picked) => fleetUpdate.start(target, picked),
})

export const panelUpdate = {
  dismiss: updater.dismiss,
  /** The old page holds still while this is true: Fleet UI is about to reload it. */
  inFlight: updater.inFlight,
}

export const fleetUpgrade = {
  start: (req: UpgradeRequest) => void upgrade.start(req),
  resume: () => void upgrade.resume(),
  dismiss: upgrade.dismiss,
}

export const updateOutcome = {
  show: (version: string) => void outcome.show(version),
  pending: outcome.pending,
  dismiss: outcome.dismiss,
}

export const afterPanel = {
  resume: (version: string, allowed: boolean) => void handover.resume(version, allowed),
  pending: handover.pending,
  dismiss: handover.dismiss,
}

export function usePanelUpdate(): PanelUpdate {
  return useSyncExternalStore(updater.subscribe, updater.get)
}

export function useFleetUpgrade(): FleetUpgrade {
  return useSyncExternalStore(upgrade.subscribe, upgrade.get)
}

export function useUpdateOutcome(): FleetOutcome {
  return useSyncExternalStore(outcome.subscribe, outcome.get)
}

export function useAfterPanel(): AfterPanel {
  return useSyncExternalStore(handover.subscribe, handover.get)
}

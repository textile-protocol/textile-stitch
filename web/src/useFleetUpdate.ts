// The Update all run, kept outside any page.
//
// The loop lives at module level so opening a bot's page mid-run doesn't stop
// it: Fleet subscribes, it doesn't own. Closing the tab does stop it. The bot
// being updated finishes on the server (the request is already in), and the
// rest stay on the build they had.
//
// A bot that fails its update, or doesn't stay up after it, is marked failed
// and the run moves on. Failures are listed together when it ends.

import { useSyncExternalStore } from 'react'
import { ApiError, api } from './api'
import {
  type FleetUpdateRun,
  type UpdateCandidate,
  finishRun,
  healthVerdict,
  nextQueued,
  runActive,
  setStep,
  startRun,
} from './fleetUpdate'
import type { BotState } from './types'

/** How long a recreated bot gets to stay up before the run calls it down. */
const HEALTH_TIMEOUT_MS = 60_000
const HEALTH_POLL_MS = 2_000

/** Everything the loop touches outside itself, so tests can hand in fakes. */
export interface RunnerDeps {
  /** POST /api/bots/{name}/update. Resolves with the server's message. */
  updateBot: (name: string) => Promise<string | null>
  /** The bot's Docker state right now. */
  readState: (name: string) => Promise<BotState>
  sleep: (ms: number) => Promise<void>
  now: () => number
  /** Called with true when a run starts and false when it ends. */
  onBusy?: (busy: boolean) => void
  healthTimeoutMs?: number
  healthPollMs?: number
}

export interface FleetUpdateRunner {
  /** Resolves when the run ends. A second start while one is going is ignored. */
  start: (target: string, picked: UpdateCandidate[]) => Promise<void>
  /** Finish the bot in flight, then stop. */
  stop: () => void
  /** Clear a finished run. */
  dismiss: () => void
  get: () => FleetUpdateRun | null
  subscribe: (listener: () => void) => () => void
}

export function createFleetUpdateRunner(deps: RunnerDeps): FleetUpdateRunner {
  const timeout = deps.healthTimeoutMs ?? HEALTH_TIMEOUT_MS
  const pollMs = deps.healthPollMs ?? HEALTH_POLL_MS
  let run: FleetUpdateRun | null = null
  const listeners = new Set<() => void>()

  const set = (next: FleetUpdateRun | null) => {
    run = next
    listeners.forEach((l) => l())
  }
  // Re-read through a function: `set` replaces `run`, and the loop must see
  // a stop pressed while it was awaiting.
  const current = () => run

  /** Null when the bot came back, else what went wrong. */
  async function waitForHealth(name: string, wantsUp: boolean): Promise<string | null> {
    const seen: BotState[] = []
    const deadline = deps.now() + timeout
    while (deps.now() < deadline) {
      try {
        seen.push(await deps.readState(name))
      } catch {
        // One failed read isn't a verdict; the deadline is.
      }
      const verdict = healthVerdict(wantsUp, seen)
      if (verdict === 'up') return null
      if (verdict === 'down') return `It stopped after the update (${seen[seen.length - 1]}).`
      await deps.sleep(pollMs)
    }
    return `It didn't stay up within ${Math.round(timeout / 1000)}s of the update.`
  }

  async function updateOne(name: string, wantsUp: boolean): Promise<void> {
    set(setStep(current()!, name, 'updating'))
    try {
      const message = await deps.updateBot(name)
      set(setStep(current()!, name, 'checking', message))
      const problem = await waitForHealth(name, wantsUp)
      set(setStep(current()!, name, problem ? 'failed' : 'done', problem ?? message))
    } catch (e) {
      const message = e instanceof ApiError ? e.message : String(e)
      set(setStep(current()!, name, 'failed', message))
    }
  }

  async function loop(): Promise<void> {
    for (let step = nextQueued(current()!); step; step = nextQueued(current()!)) {
      if (current()!.phase === 'stopping') break
      await updateOne(step.name, step.wantsUp)
    }
    const last = current()!
    set(finishRun(last, last.phase === 'stopping' ? 'cancelled' : 'done'))
  }

  return {
    async start(target, picked) {
      if (runActive(run) || picked.length === 0) return
      set(startRun(target, picked))
      deps.onBusy?.(true)
      try {
        await loop()
      } finally {
        deps.onBusy?.(false)
      }
    },
    stop() {
      if (run?.phase === 'running') set({ ...run, phase: 'stopping' })
    },
    dismiss() {
      if (!runActive(run)) set(null)
    },
    get: () => run,
    subscribe(listener) {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
  }
}

/** Warn before closing the tab mid-run: the rest of the fleet wouldn't update. */
function onBeforeUnload(e: BeforeUnloadEvent) {
  e.preventDefault()
}

const runner = createFleetUpdateRunner({
  updateBot: async (name) => (await api.updateBot(name)).message,
  readState: async (name) => (await api.bot(name)).state,
  sleep: (ms) => new Promise((r) => setTimeout(r, ms)),
  now: () => Date.now(),
  onBusy(busy) {
    if (busy) {
      window.addEventListener('beforeunload', onBeforeUnload)
      return
    }
    window.removeEventListener('beforeunload', onBeforeUnload)
    // Every bot moved, so the registry check is stale.
    void api.updates(true).catch(() => undefined)
  },
})

export const fleetUpdate = {
  start: (target: string, picked: UpdateCandidate[]) => void runner.start(target, picked),
  stop: runner.stop,
  dismiss: runner.dismiss,
}

export function useFleetUpdate(): FleetUpdateRun | null {
  return useSyncExternalStore(runner.subscribe, runner.get)
}

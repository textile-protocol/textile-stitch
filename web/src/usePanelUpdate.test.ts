import { describe, expect, it } from 'vitest'
import { ApiError } from './api'
import { finishRun, setStep, startRun, type FleetUpdateRun, type UpdateCandidate } from './fleetUpdate'
import { HANDOFF_KEY, HANDOFF_TTL_MS, encodeHandoff } from './panelHandoff'
import type { Bot, UpdatesStatus } from './types'
import { OUTCOME_KEY, OUTCOME_TTL_MS, decodeOutcome, encodeOutcome } from './updateOutcome'
import {
  createFleetUpgrade,
  createHandover,
  createOutcome,
  createPanelUpdater,
  type KeyValueStore,
  type Ping,
  type UpdateRequest,
  type UpdaterDeps,
} from './usePanelUpdate'

const PANEL = 'ghcr.io/textile-protocol/textile-stitch-panel:latest'
const BOT = 'ghcr.io/textile-protocol/textile-stitch:latest'

function memory(): KeyValueStore & { data: Map<string, string> } {
  const data = new Map<string, string>()
  return {
    data,
    getItem: (k) => data.get(k) ?? null,
    setItem: (k, v) => void data.set(k, v),
    removeItem: (k) => void data.delete(k),
  }
}

type Answer = 'down' | 'signed-out' | string

/**
 * A Fleet UI that answers with `answers` in turn: a version string (signed
 * in), 'signed-out' (the new version, without our session), or 'down' while
 * it restarts. Time only moves when the updater sleeps.
 */
function fakePanel(answers: Answer[], opts: Partial<UpdaterDeps> = {}) {
  let clock = 0
  let reloads = 0
  let polls = 0
  const storage = memory()
  const deps: UpdaterDeps = {
    updatePanel: async () => ({ targetImage: PANEL }),
    async ping(): Promise<Ping> {
      const a = answers[Math.min(polls++, answers.length - 1)]!
      if (a === 'down') throw new Error('connection refused')
      if (a === 'signed-out') return { version: '0.1.405', authenticated: false }
      return { version: a, authenticated: true }
    },
    reload: () => void reloads++,
    storage,
    sleep: async (ms) => void (clock += ms),
    now: () => clock,
    pollMs: 1_000,
    pollAttempts: 20,
    graceMs: 3_000,
    ...opts,
  }
  return { deps, storage, reloads: () => reloads, polls: () => polls }
}

const req = (bots = 0, signsOut = false): UpdateRequest => ({ from: '0.1.400', bots, failed: 0, signsOut })

describe('createPanelUpdater', () => {
  it('reloads once Fleet UI drops out and comes back', async () => {
    const p = fakePanel(['0.1.400', 'down', 'down', '0.1.405'])
    const updater = createPanelUpdater(p.deps)
    await updater.start(req())
    expect(p.reloads()).toBe(1)
    expect(updater.get()).toEqual({ phase: 'reloading', bots: 0, signsOut: false })
  })

  it('reloads the moment the new Fleet UI no longer knows our session', async () => {
    // A password session: the old version still answers once, then the new
    // one turns us away. The reload happens right then, so the sign-in form
    // only ever shows on the new page.
    const p = fakePanel(['0.1.400', 'signed-out'], { graceMs: 60_000 })
    const updater = createPanelUpdater(p.deps)
    await updater.start(req(0, true))
    expect(p.polls()).toBe(2)
    expect(p.reloads()).toBe(1)
  })

  it('reloads as soon as the new version answers, without seeing a drop', async () => {
    const p = fakePanel(['0.1.405'])
    await createPanelUpdater(p.deps).start(req())
    expect(p.polls()).toBe(1)
    expect(p.reloads()).toBe(1)
  })

  it('waits out the grace when nothing tells the restart apart', async () => {
    const p = fakePanel(['0.1.400'])
    await createPanelUpdater(p.deps).start(req())
    expect(p.polls()).toBe(3)
    expect(p.reloads()).toBe(1)
  })

  it('leaves a note saying how many bots went first, for the next page', async () => {
    const p = fakePanel(['down', '0.1.405'])
    const updater = createPanelUpdater(p.deps)
    await updater.start(req(4))
    expect(decodeOutcome(p.storage.getItem(OUTCOME_KEY), 0)).toEqual({ from: '0.1.400', bots: 4, failed: 0, at: 0 })
    expect(updater.inFlight()).toBe(true)
  })

  it('leaves no note and stays put when the update is refused', async () => {
    const p = fakePanel(['0.1.400'], {
      updatePanel: async () => {
        throw new ApiError(500, 'manifest unknown', null)
      },
    })
    const updater = createPanelUpdater(p.deps)
    await updater.start(req(2))
    expect(updater.get()).toEqual({ phase: 'failed', error: 'manifest unknown', bots: 2, signsOut: false })
    expect(updater.inFlight()).toBe(false)
    expect(p.storage.data.size).toBe(0)
    expect(p.reloads()).toBe(0)
  })

  it('still updates without sessionStorage; only the next page says less', async () => {
    const p = fakePanel(['0.1.405'], { storage: null })
    await createPanelUpdater(p.deps).start(req(2))
    expect(p.reloads()).toBe(1)
  })

  it("gives up as slow when Fleet UI doesn't come back", async () => {
    const p = fakePanel(['down'])
    const updater = createPanelUpdater(p.deps)
    await updater.start(req())
    expect(updater.get()).toMatchObject({ phase: 'slow' })
    expect(updater.inFlight()).toBe(false)
    expect(p.reloads()).toBe(0)
  })

  it('ignores a second start while one is under way', async () => {
    let release: () => void = () => {}
    const p = fakePanel(['down', '0.1.405'])
    let calls = 0
    const updater = createPanelUpdater({
      ...p.deps,
      updatePanel: () => {
        calls++
        return new Promise((r) => (release = () => r({ targetImage: PANEL })))
      },
    })
    const first = updater.start(req())
    await updater.start(req())
    release()
    await first
    expect(calls).toBe(1)
  })
})

function candidate(name: string): UpdateCandidate {
  return { name, label: name, image: null, version: null, wantsUp: true }
}

/** A run that ends with these statuses, or is stopped after them. */
function endedRun(statuses: ('done' | 'failed')[], phase: 'done' | 'cancelled' = 'done'): FleetUpdateRun {
  const run = statuses.reduce(
    (r, status, i) => setStep(r, `b${i}`, status),
    startRun(BOT, statuses.map((_, i) => candidate(`b${i}`))),
  )
  return finishRun(run, phase)
}

function fakeUpgrade(run: FleetUpdateRun | null) {
  const order: string[] = []
  const uiRequests: UpdateRequest[] = []
  const upgrade = createFleetUpgrade({
    async runBots(_target, picked) {
      order.push(`bots:${picked.length}`)
      return run
    },
    async updateFleetUi(r) {
      order.push('fleet-ui')
      uiRequests.push(r)
    },
  })
  return { upgrade, order, uiRequests }
}

const upgradeReq = (picked: UpdateCandidate[]) => ({
  target: BOT,
  picked,
  from: '0.1.400',
  signsOut: true,
})

describe('createFleetUpgrade', () => {
  it('updates the bots first and Fleet UI only after every one came back', async () => {
    const f = fakeUpgrade(endedRun(['done', 'done', 'done']))
    await f.upgrade.start(upgradeReq([candidate('a'), candidate('b'), candidate('c')]))
    expect(f.order).toEqual(['bots:3', 'fleet-ui'])
    expect(f.uiRequests).toEqual([{ from: '0.1.400', bots: 3, failed: 0, signsOut: true }])
    expect(f.upgrade.get()).toEqual({ phase: 'idle' })
  })

  it('goes straight to Fleet UI when no bots are ticked', async () => {
    const f = fakeUpgrade(null)
    await f.upgrade.start(upgradeReq([]))
    expect(f.order).toEqual(['fleet-ui'])
    expect(f.uiRequests[0]?.bots).toBe(0)
  })

  it("holds Fleet UI when a bot didn't come back, so nobody is signed out mid-fix", async () => {
    const f = fakeUpgrade(endedRun(['done', 'failed', 'done']))
    await f.upgrade.start(upgradeReq([candidate('a'), candidate('b'), candidate('c')]))
    expect(f.order).toEqual(['bots:3'])
    expect(f.upgrade.get()).toEqual({
      phase: 'held',
      from: '0.1.400',
      signsOut: true,
      done: 2,
      failed: 1,
      total: 3,
      stopped: false,
    })
    // The operator can go on once they've dealt with it.
    await f.upgrade.resume()
    expect(f.order).toEqual(['bots:3', 'fleet-ui'])
    expect(f.uiRequests[0]).toMatchObject({ bots: 2, failed: 1 })
  })

  it('holds Fleet UI when the operator stopped the bots', async () => {
    const f = fakeUpgrade(endedRun(['done'], 'cancelled'))
    await f.upgrade.start(upgradeReq([candidate('a'), candidate('b')]))
    expect(f.upgrade.get()).toMatchObject({ phase: 'held', stopped: true })
    f.upgrade.dismiss()
    expect(f.upgrade.get()).toEqual({ phase: 'idle' })
    expect(f.order).toEqual(['bots:2'])
  })

  it('leaves Fleet UI alone when the runner was busy with another run', async () => {
    const f = fakeUpgrade(null)
    await f.upgrade.start(upgradeReq([candidate('a')]))
    expect(f.order).toEqual(['bots:1'])
    expect(f.upgrade.get()).toEqual({ phase: 'idle' })
  })
})

function fakeOutcome(opts: {
  note?: { from: string; bots: number; failed?: number; at?: number }
  now?: number
  updates?: UpdatesStatus | Error
}) {
  const storage = memory()
  if (opts.note) {
    storage.setItem(
      OUTCOME_KEY,
      encodeOutcome({ failed: 0, ...opts.note, at: opts.note.at ?? 0 }),
    )
  }
  const outcome = createOutcome({
    storage,
    now: () => opts.now ?? 1_000,
    async updates() {
      if (opts.updates instanceof Error) throw opts.updates
      return opts.updates ?? status([])
    },
  })
  return { outcome, storage }
}

describe('createOutcome', () => {
  it('says what Fleet UI moved from, once, and takes the note', async () => {
    const o = fakeOutcome({ note: { from: '0.1.400', bots: 4 } })
    await o.outcome.show('0.1.405')
    expect(o.outcome.get()).toEqual({ phase: 'updated', from: '0.1.400', version: '0.1.405', bots: 4, failed: 0 })
    expect(o.storage.data.size).toBe(0)
  })

  it('warns when Fleet UI came back on the same version still reporting an update', async () => {
    const o = fakeOutcome({ note: { from: '0.1.400', bots: 0 }, updates: status([], true) })
    await o.outcome.show('0.1.400')
    expect(o.outcome.get()).toEqual({ phase: 'not-moved', bots: 0 })
  })

  it('counts a same-number re-release as updated when nothing is pending', async () => {
    const o = fakeOutcome({ note: { from: '0.1.400', bots: 0 } })
    await o.outcome.show('0.1.400')
    expect(o.outcome.get()).toMatchObject({ phase: 'updated' })
  })

  it("says nothing when it can't tell, or the note is stale", async () => {
    const failing = fakeOutcome({ note: { from: '0.1.400', bots: 0 }, updates: new Error('down') })
    await failing.outcome.show('0.1.400')
    expect(failing.outcome.get()).toEqual({ phase: 'none' })
    const stale = fakeOutcome({ note: { from: '0.1.400', bots: 1 }, now: OUTCOME_TTL_MS + 1 })
    await stale.outcome.show('0.1.405')
    expect(stale.outcome.get()).toEqual({ phase: 'none' })
  })

  it('tells the sign-in screen a note is waiting, without taking it', () => {
    const o = fakeOutcome({ note: { from: '0.1.400', bots: 2 } })
    expect(o.outcome.pending()).toBe(true)
    expect(o.storage.data.size).toBe(1)
    expect(fakeOutcome({}).outcome.pending()).toBe(false)
  })
})

// The planner only reads these fields.
const bot = (name: string) =>
  ({
    name,
    displayName: null,
    layout: 'per-bot-dir',
    container: `stitch-${name}`,
    state: 'running',
    image: BOT,
    version: 'v0.1.233',
    editable: true,
    canMigrate: false,
    warnings: [],
  }) as unknown as Bot

function status(behind: string[], panelBehind = false): UpdatesStatus {
  const info = { targetImage: BOT, currentImage: BOT, reason: null }
  return {
    bot: { ...info, updateAvailable: behind.length > 0 },
    panel: { ...info, targetImage: PANEL, updateAvailable: panelBehind },
    bots: behind.map((name) => ({ name, currentImage: BOT, updateAvailable: true, canUpdate: true })),
  }
}

function fakeHandover(opts: {
  note?: string[]
  at?: number
  now?: number
  fleet?: Bot[]
  updates?: UpdatesStatus | Error
}) {
  const storage = memory()
  if (opts.note) {
    storage.setItem(HANDOFF_KEY, encodeHandoff({ bots: opts.note, from: '0.1.233', at: opts.at ?? 0 }))
  }
  const runs: { target: string; picked: UpdateCandidate[] }[] = []
  const handover = createHandover({
    storage,
    now: () => opts.now ?? 1_000,
    fleet: async () => opts.fleet ?? [],
    async updates() {
      if (opts.updates instanceof Error) throw opts.updates
      return opts.updates ?? status([])
    },
    startRun: (target, picked) => void runs.push({ target, picked }),
  })
  return { handover, storage, runs }
}

describe('createHandover (from an older page)', () => {
  it('starts a run with the ticked bots that are still behind', async () => {
    const h = fakeHandover({
      note: ['a', 'b'],
      fleet: [bot('a'), bot('b'), bot('c')],
      updates: status(['b', 'c']),
    })
    await h.handover.resume('0.1.240', true)
    expect(h.runs.map((r) => r.picked.map((c) => c.name))).toEqual([['b']])
    expect(h.runs[0]?.target).toBe(BOT)
    expect(h.handover.get()).toEqual({ phase: 'bots', from: '0.1.233', version: '0.1.240' })
  })

  it('takes the note before acting, and acts once per page load', async () => {
    const h = fakeHandover({ note: ['a'], fleet: [bot('a')], updates: status(['a']) })
    await Promise.all([h.handover.resume('0.1.240', true), h.handover.resume('0.1.240', true)])
    expect(h.storage.data.size).toBe(0)
    expect(h.runs).toHaveLength(1)
  })

  it('does nothing without a note', async () => {
    const h = fakeHandover({ fleet: [bot('a')], updates: status(['a']) })
    await h.handover.resume('0.1.240', true)
    expect(h.runs).toHaveLength(0)
    expect(h.handover.get()).toEqual({ phase: 'none' })
  })

  it('drops the note unread where bulk bot updates are off', async () => {
    const h = fakeHandover({ note: ['a'], fleet: [bot('a')], updates: status(['a']) })
    await h.handover.resume('0.1.240', false)
    expect(h.runs).toHaveLength(0)
    expect(h.storage.data.size).toBe(0)
    expect(h.handover.get()).toEqual({ phase: 'none' })
  })

  it("doesn't run a stale note", async () => {
    const h = fakeHandover({ note: ['a'], at: 0, now: HANDOFF_TTL_MS + 1, fleet: [bot('a')], updates: status(['a']) })
    await h.handover.resume('0.1.240', true)
    expect(h.runs).toHaveLength(0)
    expect(h.handover.get()).toMatchObject({ phase: 'skipped', reason: 'expired' })
  })

  it("leaves the bots alone when the panel didn't move", async () => {
    const h = fakeHandover({ note: ['a'], fleet: [bot('a')], updates: status(['a'], true) })
    await h.handover.resume('0.1.233', true)
    expect(h.runs).toHaveLength(0)
    expect(h.handover.get()).toMatchObject({ phase: 'skipped', reason: 'panel-not-moved' })
  })

  it('reports a failed check instead of guessing', async () => {
    const h = fakeHandover({ note: ['a'], fleet: [bot('a')], updates: new ApiError(502, 'docker down', null) })
    await h.handover.resume('0.1.240', true)
    expect(h.runs).toHaveLength(0)
    expect(h.handover.get()).toMatchObject({ phase: 'skipped', reason: 'check-failed', error: 'docker down' })
  })

  it('tells the sign-in screen how many bots wait, without taking the note', () => {
    const h = fakeHandover({ note: ['a', 'b'] })
    expect(h.handover.pending()).toBe(2)
    expect(h.storage.data.size).toBe(1)
    expect(fakeHandover({ note: ['a'], at: 0, now: HANDOFF_TTL_MS + 1 }).handover.pending()).toBe(0)
  })

  it("can't dismiss while it's still checking", async () => {
    let release: (u: UpdatesStatus) => void = () => {}
    const storage = memory()
    storage.setItem(HANDOFF_KEY, encodeHandoff({ bots: ['a'], from: 'x', at: 0 }))
    const handover = createHandover({
      storage,
      now: () => 0,
      fleet: async () => [bot('a')],
      updates: () => new Promise((r) => (release = r)),
      startRun: () => {},
    })
    const going = handover.resume('y', true)
    await Promise.resolve()
    handover.dismiss()
    expect(handover.get().phase).toBe('checking')
    release(status([]))
    await going
    handover.dismiss()
    expect(handover.get().phase).toBe('none')
  })
})

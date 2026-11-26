import { describe, expect, it } from 'vitest'
import { ApiError } from './api'
import type { UpdateCandidate } from './fleetUpdate'
import { HANDOFF_KEY, HANDOFF_TTL_MS, decodeHandoff, encodeHandoff } from './panelHandoff'
import type { Bot, UpdatesStatus } from './types'
import {
  createHandover,
  createPanelUpdater,
  type KeyValueStore,
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

/**
 * A panel that answers with `answers` in turn: a version string, or 'down'
 * while it's being swapped. Time only moves when the updater sleeps.
 */
function fakePanel(answers: ('down' | string)[], opts: Partial<UpdaterDeps> = {}) {
  let clock = 0
  let reloads = 0
  let polls = 0
  const storage = memory()
  const deps: UpdaterDeps = {
    updatePanel: async () => ({ targetImage: PANEL }),
    async ping() {
      const a = answers[Math.min(polls++, answers.length - 1)]
      if (a === 'down') throw new Error('connection refused')
      return a!
    },
    reload: () => void reloads++,
    storage,
    sleep: async (ms) => void (clock += ms),
    now: () => clock,
    pollMs: 1_000,
    pollAttempts: 20,
    graceMs: 3_000,
    graceWithBotsMs: 10_000,
    ...opts,
  }
  return { deps, storage, reloads: () => reloads, polls: () => polls }
}

describe('createPanelUpdater', () => {
  it('reloads once the panel drops out and comes back', async () => {
    const p = fakePanel(['0.1.233', 'down', 'down', '0.1.240'])
    const updater = createPanelUpdater(p.deps)
    await updater.start({ bots: [], from: '0.1.233' })
    expect(p.reloads()).toBe(1)
    expect(updater.get()).toEqual({ phase: 'reloading', bots: 0 })
    expect(p.storage.data.size).toBe(0)
  })

  it('leaves the ticked bots for the new page once the swap is armed', async () => {
    const p = fakePanel(['down', '0.1.240'])
    const updater = createPanelUpdater(p.deps)
    await updater.start({ bots: ['a', 'b'], from: '0.1.233' })
    const read = decodeHandoff(p.storage.getItem(HANDOFF_KEY), 0)
    expect(read).toEqual({ kind: 'ready', handoff: { bots: ['a', 'b'], from: '0.1.233', at: 0 } })
    expect(updater.get()).toMatchObject({ phase: 'reloading', bots: 2 })
  })

  it('leaves no note when the panel update is refused', async () => {
    const p = fakePanel(['0.1.233'], {
      updatePanel: async () => {
        throw new ApiError(500, 'manifest unknown', null)
      },
    })
    const updater = createPanelUpdater(p.deps)
    await updater.start({ bots: ['a'], from: '0.1.233' })
    expect(updater.get()).toEqual({ phase: 'failed', error: 'manifest unknown', bots: 1 })
    expect(p.storage.data.size).toBe(0)
    expect(p.reloads()).toBe(0)
  })

  it('refuses to start with bots ticked when there is nowhere to keep them', async () => {
    const p = fakePanel(['0.1.240'], { storage: null })
    let called = false
    const updater = createPanelUpdater({
      ...p.deps,
      updatePanel: async () => {
        called = true
        return { targetImage: PANEL }
      },
    })
    await updater.start({ bots: ['a'], from: '0.1.233' })
    expect(updater.get()).toMatchObject({ phase: 'failed' })
    expect(called).toBe(false)
  })

  it('reloads as soon as the new version answers, without seeing a drop', async () => {
    const p = fakePanel(['0.1.240'])
    await createPanelUpdater(p.deps).start({ bots: ['a'], from: '0.1.233' })
    expect(p.polls()).toBe(1)
    expect(p.reloads()).toBe(1)
  })

  it('waits longer for the swap when bots are handed over', async () => {
    // Same version, never seen down: only the grace lets it reload.
    const alone = fakePanel(['0.1.233'])
    await createPanelUpdater(alone.deps).start({ bots: [], from: '0.1.233' })
    const withBots = fakePanel(['0.1.233'])
    await createPanelUpdater(withBots.deps).start({ bots: ['a'], from: '0.1.233' })
    expect(alone.polls()).toBe(3)
    expect(withBots.polls()).toBe(10)
  })

  it("gives up as slow when the panel doesn't come back", async () => {
    const p = fakePanel(['down'])
    const updater = createPanelUpdater(p.deps)
    await updater.start({ bots: ['a'], from: '0.1.233' })
    expect(updater.get()).toMatchObject({ phase: 'slow', bots: 1 })
    expect(p.reloads()).toBe(0)
    // The note stays, so Reload now can still hand the bots over.
    expect(p.storage.data.has(HANDOFF_KEY)).toBe(true)
  })

  it('ignores a second start while one is under way', async () => {
    let release: () => void = () => {}
    const p = fakePanel(['down', '0.1.240'])
    let calls = 0
    const updater = createPanelUpdater({
      ...p.deps,
      updatePanel: () => {
        calls++
        return new Promise((r) => (release = () => r({ targetImage: PANEL })))
      },
    })
    const first = updater.start({ bots: [], from: '0.1.233' })
    await updater.start({ bots: [], from: '0.1.233' })
    release()
    await first
    expect(calls).toBe(1)
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

describe('createHandover', () => {
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

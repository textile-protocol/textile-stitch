import { describe, expect, it } from 'vitest'
import {
  STABLE_READS,
  defaultSelection,
  finishRun,
  healthVerdict,
  isPinnedImage,
  nextQueued,
  planFleetUpdate,
  runActive,
  runCounts,
  setStep,
  startRun,
  wantsToBeUp,
  type UpdateCandidate,
} from './fleetUpdate'
import type { Bot, BotState, UpdatesStatus } from './types'

const LATEST = 'ghcr.io/textile-protocol/textile-stitch:latest'
const PIN = 'ghcr.io/textile-protocol/textile-stitch:sha-14cd877'

// Only the fields the planner reads; the rest of a Bot doesn't matter here.
function bot(name: string, opts: Partial<Bot> = {}): Bot {
  return {
    name,
    displayName: null,
    origin: 'panel',
    layout: 'per-bot-dir',
    container: `stitch-${name}`,
    state: 'running',
    status: 'Up 3 days',
    running: true,
    canStop: true,
    image: LATEST,
    version: 'v0.1.231',
    editable: true,
    canMigrate: false,
    migrateBlockedReason: null,
    canApprove: true,
    custodyApprovals: false,
    approveBlockedReason: null,
    approveBlockedBy: null,
    canWithdraw: false,
    withdrawBlockedReason: null,
    withdrawBlockedBy: null,
    config: null,
    warnings: [],
    ...opts,
  }
}

function updates(behind: string[], canUpdate: string[] = behind): UpdatesStatus {
  const info = { targetImage: LATEST, currentImage: LATEST, updateAvailable: true, reason: null }
  const names = [...new Set([...behind, ...canUpdate])]
  return {
    bot: info,
    panel: { ...info, updateAvailable: false },
    bots: names.map((name) => ({
      name,
      currentImage: LATEST,
      updateAvailable: behind.includes(name),
      canUpdate: canUpdate.includes(name),
    })),
  }
}

function candidate(name: string, wantsUp = true): UpdateCandidate {
  return { name, label: name, image: LATEST, version: null, wantsUp }
}

describe('isPinnedImage', () => {
  it('matches the server: sha-* tags, digest refs and bare ids are pins', () => {
    expect(isPinnedImage(PIN)).toBe(true)
    expect(isPinnedImage('ghcr.io/textile-protocol/textile-stitch@sha256:abc')).toBe(true)
    expect(isPinnedImage('sha256:ce1d74')).toBe(true)
  })

  it('treats mutable tags, a missing tag and no image as unpinned', () => {
    expect(isPinnedImage(LATEST)).toBe(false)
    expect(isPinnedImage('ghcr.io/textile-protocol/textile-stitch:canary')).toBe(false)
    expect(isPinnedImage('localhost:5000/stitch')).toBe(false)
    expect(isPinnedImage(null)).toBe(false)
  })
})

describe('wantsToBeUp', () => {
  it('counts restarting and paused bots as meant to be up', () => {
    const up: BotState[] = ['running', 'restarting', 'paused']
    const down: BotState[] = ['created', 'exited', 'dead', 'unknown']
    expect(up.every(wantsToBeUp)).toBe(true)
    expect(down.some(wantsToBeUp)).toBe(false)
  })
})

describe('planFleetUpdate', () => {
  it('lists only bots the registry check says are behind, in fleet order', () => {
    const bots = [bot('c'), bot('a'), bot('b')]
    const plan = planFleetUpdate(bots, updates(['b', 'c']))
    expect(plan.ready.map((c) => c.name)).toEqual(['c', 'b'])
    expect(plan.skipped).toEqual([])
  })

  it('ignores a pin the server would let Update leave but that is not behind', () => {
    const plan = planFleetUpdate([bot('p', { image: PIN })], updates([], ['p']))
    expect(plan.ready).toEqual([])
    expect(plan.skipped).toEqual([])
  })

  it('needs canUpdate as well as updateAvailable', () => {
    const plan = planFleetUpdate([bot('a')], updates(['a'], []))
    expect(plan.ready).toEqual([])
  })

  it('leaves a pinned bot pinned, with a reason, even when it is behind', () => {
    const plan = planFleetUpdate([bot('p', { image: PIN }), bot('a')], updates(['p', 'a']))
    expect(plan.ready.map((c) => c.name)).toEqual(['a'])
    expect(plan.skipped).toHaveLength(1)
    expect(plan.skipped[0]?.name).toBe('p')
    expect(plan.skipped[0]?.reason).toMatch(/Pinned/)
  })

  it('skips the bots the single-bot Update would refuse, saying why', () => {
    const bots = [
      bot('flat', { layout: 'flat-files' }),
      bot('migrate', { canMigrate: true }),
      bot('gone', { container: null }),
      bot('locked', {
        editable: false,
        warnings: [{ kind: 'x', message: 'Config is read-only.', blocksEditing: true }],
      }),
    ]
    const plan = planFleetUpdate(bots, updates(bots.map((b) => b.name)))
    expect(plan.ready).toEqual([])
    const reasons = Object.fromEntries(plan.skipped.map((s) => [s.name, s.reason]))
    expect(reasons.flat).toMatch(/flat layout/)
    expect(reasons.migrate).toMatch(/flat layout/)
    expect(reasons.gone).toMatch(/no container/)
    expect(reasons.locked).toBe('Config is read-only.')
  })

  it('uses the display name and records whether the bot comes back up', () => {
    const bots = [bot('a', { displayName: 'Lagos main' }), bot('b', { state: 'exited' })]
    const plan = planFleetUpdate(bots, updates(['a', 'b']))
    expect(plan.ready.map((c) => [c.label, c.wantsUp])).toEqual([
      ['Lagos main', true],
      ['b', false],
    ])
  })

  it('plans nothing before the registry check has answered', () => {
    expect(planFleetUpdate([bot('a')], null)).toEqual({ ready: [], skipped: [] })
  })

  it('ticks every bot it can update by default', () => {
    const plan = planFleetUpdate([bot('a'), bot('b')], updates(['a', 'b']))
    expect(defaultSelection(plan)).toEqual(['a', 'b'])
  })
})

describe('run state', () => {
  it('starts with every bot queued and walks them in order', () => {
    const run = startRun(LATEST, [candidate('a'), candidate('b')])
    expect(runActive(run)).toBe(true)
    expect(nextQueued(run)?.name).toBe('a')
    const next = setStep(run, 'a', 'done')
    expect(nextQueued(next)?.name).toBe('b')
    expect(run.steps[0]?.status).toBe('queued')
  })

  it('counts done and failed bots as settled', () => {
    let run = startRun(LATEST, [candidate('a'), candidate('b'), candidate('c')])
    run = setStep(run, 'a', 'done')
    run = setStep(run, 'b', 'failed', 'boom')
    expect(runCounts(run)).toEqual({ done: 1, failed: 1, settled: 2, total: 3 })
    expect(run.steps[1]?.detail).toBe('boom')
  })

  it('marks whatever never started as untouched when the run ends', () => {
    let run = startRun(LATEST, [candidate('a'), candidate('b')])
    run = finishRun(setStep(run, 'a', 'done'), 'cancelled')
    expect(run.steps.map((s) => s.status)).toEqual(['done', 'untouched'])
    expect(runActive(run)).toBe(false)
    expect(runActive(null)).toBe(false)
  })
})

describe('healthVerdict', () => {
  const running = (n: number): BotState[] => Array.from({ length: n }, () => 'running')

  it('needs STABLE_READS running reads in a row', () => {
    expect(healthVerdict(true, running(STABLE_READS - 1))).toBe('waiting')
    expect(healthVerdict(true, running(STABLE_READS))).toBe('up')
  })

  it('starts counting again after a restart', () => {
    const seen: BotState[] = [...running(STABLE_READS - 1), 'restarting', ...running(2)]
    expect(healthVerdict(true, seen)).toBe('waiting')
  })

  it('calls exited or dead down at once', () => {
    expect(healthVerdict(true, ['restarting', 'exited'])).toBe('down')
    expect(healthVerdict(true, [...running(3), 'dead'])).toBe('down')
  })

  it('has nothing to wait for on a bot that was stopped before', () => {
    expect(healthVerdict(false, [])).toBe('up')
  })

  it('waits before the first read lands', () => {
    expect(healthVerdict(true, [])).toBe('waiting')
  })
})

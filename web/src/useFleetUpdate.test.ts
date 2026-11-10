import { describe, expect, it } from 'vitest'
import { ApiError } from './api'
import type { UpdateCandidate } from './fleetUpdate'
import type { BotState } from './types'
import { createFleetUpdateRunner, type RunnerDeps } from './useFleetUpdate'

const TARGET = 'ghcr.io/textile-protocol/textile-stitch:latest'

function candidate(name: string, wantsUp = true): UpdateCandidate {
  return { name, label: name, image: null, version: null, wantsUp }
}

/**
 * A fleet whose bots come back up after an update unless listed in `crash`.
 * Time only moves when the runner sleeps, so a health timeout costs nothing.
 */
function fakeFleet(opts: { crash?: string[]; refuse?: string[]; neverSettles?: string[] } = {}) {
  let clock = 0
  const updated: string[] = []
  const deps: RunnerDeps = {
    async updateBot(name) {
      if (opts.refuse?.includes(name)) {
        throw new ApiError(409, `${name} still uses the flat file layout.`, null)
      }
      updated.push(name)
      return `${name} was recreated on ${TARGET}.`
    },
    async readState(name): Promise<BotState> {
      if (opts.crash?.includes(name)) return 'exited'
      if (opts.neverSettles?.includes(name)) return 'restarting'
      return 'running'
    },
    async sleep(ms) {
      clock += ms
    },
    now: () => clock,
    healthPollMs: 1_000,
    healthTimeoutMs: 10_000,
  }
  return { deps, updated }
}

describe('createFleetUpdateRunner', () => {
  it('updates every picked bot in order and reports them done', async () => {
    const { deps, updated } = fakeFleet()
    const runner = createFleetUpdateRunner(deps)
    await runner.start(TARGET, [candidate('a'), candidate('b', false)])
    expect(updated).toEqual(['a', 'b'])
    expect(runner.get()?.phase).toBe('done')
    expect(runner.get()?.steps.map((s) => s.status)).toEqual(['done', 'done'])
  })

  it('carries on past a bot that crashes after its update', async () => {
    const { deps, updated } = fakeFleet({ crash: ['b'] })
    const runner = createFleetUpdateRunner(deps)
    await runner.start(TARGET, [candidate('a'), candidate('b'), candidate('c')])
    expect(updated).toEqual(['a', 'b', 'c'])
    const steps = runner.get()!.steps
    expect(steps.map((s) => s.status)).toEqual(['done', 'failed', 'done'])
    expect(steps[1]?.detail).toMatch(/stopped after the update \(exited\)/)
    expect(runner.get()?.phase).toBe('done')
  })

  it('reports the server refusal as the failure and moves on', async () => {
    const { deps, updated } = fakeFleet({ refuse: ['a'] })
    const runner = createFleetUpdateRunner(deps)
    await runner.start(TARGET, [candidate('a'), candidate('b')])
    expect(updated).toEqual(['b'])
    expect(runner.get()?.steps[0]?.detail).toBe('a still uses the flat file layout.')
  })

  it('fails a bot that never settles once the timeout passes', async () => {
    const { deps } = fakeFleet({ neverSettles: ['a'] })
    const runner = createFleetUpdateRunner(deps)
    await runner.start(TARGET, [candidate('a')])
    expect(runner.get()?.steps[0]?.status).toBe('failed')
    expect(runner.get()?.steps[0]?.detail).toMatch(/didn't stay up within 10s/)
  })

  it('finishes the bot in flight on stop and leaves the rest untouched', async () => {
    const { deps, updated } = fakeFleet()
    const runner = createFleetUpdateRunner({
      ...deps,
      async updateBot(name) {
        runner.stop()
        return deps.updateBot(name)
      },
    })
    await runner.start(TARGET, [candidate('a'), candidate('b'), candidate('c')])
    expect(updated).toEqual(['a'])
    expect(runner.get()?.phase).toBe('cancelled')
    expect(runner.get()?.steps.map((s) => s.status)).toEqual(['done', 'untouched', 'untouched'])
  })

  it('ignores a second start while a run is going, and an empty pick', async () => {
    const { deps, updated } = fakeFleet()
    const runner = createFleetUpdateRunner(deps)
    await runner.start(TARGET, [])
    expect(runner.get()).toBeNull()
    const first = runner.start(TARGET, [candidate('a')])
    await runner.start(TARGET, [candidate('b')])
    await first
    expect(updated).toEqual(['a'])
  })

  it('tells the page when it is busy, and only clears a finished run', async () => {
    const busy: boolean[] = []
    const { deps } = fakeFleet()
    const runner = createFleetUpdateRunner({ ...deps, onBusy: (b) => busy.push(b) })
    let notified = 0
    runner.subscribe(() => notified++)
    const going = runner.start(TARGET, [candidate('a')])
    runner.dismiss()
    expect(runner.get()).not.toBeNull()
    await going
    expect(busy).toEqual([true, false])
    expect(notified).toBeGreaterThan(0)
    runner.dismiss()
    expect(runner.get()).toBeNull()
  })
})

import { describe, expect, it } from 'vitest'
import { finishRun, setStep, startRun, type UpdateCandidate } from './fleetUpdate'
import {
  afterPanelView,
  fleetNow,
  outcomeView,
  panelFlowBusy,
  panelUpdateBusy,
  panelUpdateView,
  upgradeView,
  type AfterPanel,
  type FleetUpgrade,
  type PanelUpdate,
} from './panelUpdate'

const BOT_TARGET = 'ghcr.io/textile-protocol/textile-stitch:latest'
const alone = { bots: 0, signsOut: false }
const afterBots = { bots: 3, signsOut: false }

function candidate(name: string): UpdateCandidate {
  return { name, label: name, image: null, version: null, wantsUp: true }
}

function ended(statuses: ('done' | 'failed')[], phase: 'done' | 'cancelled' = 'done') {
  const names = statuses.map((_, i) => `b${i}`)
  const run = statuses.reduce(
    (r, status, i) => setStep(r, `b${i}`, status),
    startRun(BOT_TARGET, names.map(candidate)),
  )
  return finishRun(run, phase)
}

describe('panelUpdateView', () => {
  it('shows nothing when no update is running', () => {
    expect(panelUpdateView({ phase: 'idle' })).toBeNull()
  })

  it('walks Download, Restart, Reconnect for Fleet UI alone', () => {
    const views = (['pulling', 'armed', 'restarting', 'reloading'] as const).map((phase) =>
      panelUpdateView({ phase, ...alone }),
    )
    expect(views.map((v) => v?.step)).toEqual([0, 1, 1, 2])
    expect(views[0]?.steps).toEqual(['Download', 'Restart', 'Reconnect'])
    expect(views[0]?.title).toBe('Updating Fleet UI')
  })

  it('puts the bots first, already ticked, when they updated before it', () => {
    const view = panelUpdateView({ phase: 'pulling', ...afterBots })
    expect(view?.steps).toEqual(['Update bots', 'Download', 'Restart', 'Reconnect'])
    expect(view?.step).toBe(1)
    expect(view?.title).toBe('Bots updated · updating Fleet UI')
    expect(panelUpdateView({ phase: 'reloading', ...afterBots })?.step).toBe(3)
  })

  it('warns a password session it signs in again', () => {
    expect(panelUpdateView({ phase: 'restarting', bots: 0, signsOut: true })?.detail).toMatch(
      /asks you to sign in again/,
    )
    expect(panelUpdateView({ phase: 'restarting', ...alone })?.detail).not.toMatch(/sign in/)
  })

  it('talks about Fleet UI and trading, never the panel or images', () => {
    const phases = ['pulling', 'armed', 'restarting', 'reloading', 'slow'] as const
    const text = phases
      .map((phase) => panelUpdateView({ phase, bots: 2, signsOut: true }))
      .map((v) => `${v?.title} ${v?.detail}`)
      .join(' ')
    expect(text).not.toMatch(/panel|image|container|registry|pull/i)
  })

  it("shows the server's error, and says the bots are fine when they went first", () => {
    expect(panelUpdateView({ phase: 'failed', error: 'no container', ...alone })).toMatchObject({
      tone: 'danger',
      detail: 'no container',
      step: null,
      dismissible: true,
    })
    expect(panelUpdateView({ phase: 'failed', error: 'no container', ...afterBots })?.detail).toBe(
      'no container Your bots are updated and trading.',
    )
  })

  it('switches to a warning with Reload when Fleet UI never came back', () => {
    expect(panelUpdateView({ phase: 'slow', ...alone })).toMatchObject({
      tone: 'warning',
      step: null,
      actions: ['reload'],
    })
  })
})

describe('upgradeView', () => {
  const bots: FleetUpgrade = { phase: 'bots', from: '0.1.400', signsOut: false }

  it('counts the bots on step one and says Fleet UI comes after', () => {
    const run = setStep(startRun(BOT_TARGET, [candidate('a'), candidate('b')]), 'a', 'done')
    const view = upgradeView(bots, run)
    expect(view).toMatchObject({
      tone: 'progress',
      title: 'Updating bots · 1 of 2',
      step: 0,
      steps: ['Update bots', 'Download', 'Restart', 'Reconnect'],
      dismissible: false,
    })
    expect(view?.detail).toMatch(/Fleet UI updates after the last one/)
  })

  it("says Fleet UI won't update when the run is being stopped", () => {
    const run = { ...startRun(BOT_TARGET, [candidate('a')]), phase: 'stopping' as const }
    expect(upgradeView(bots, run)?.detail).toMatch(/Fleet UI won't update/)
  })

  it('holds Fleet UI when a bot did not come back, with a way to go on', () => {
    const held: FleetUpgrade = {
      phase: 'held',
      from: '0.1.400',
      signsOut: true,
      done: 2,
      failed: 1,
      total: 3,
      stopped: false,
    }
    const away = upgradeView(held, ended(['done', 'failed', 'done']))
    expect(away).toMatchObject({
      tone: 'danger',
      title: "Fleet UI on hold · 1 of 3 bots didn't come back",
      actions: ['fleet-ui', 'fleet'],
      dismissible: true,
    })
    expect(away?.detail).toMatch(/view your bots to see which one/)
    expect(upgradeView(held, null, true)?.detail).toMatch(/open the one that failed, below/)
  })

  it('holds Fleet UI when the operator stopped the bots', () => {
    const held: FleetUpgrade = {
      phase: 'held',
      from: '0.1.400',
      signsOut: false,
      done: 1,
      failed: 0,
      total: 4,
      stopped: true,
    }
    expect(upgradeView(held, null)).toMatchObject({
      tone: 'warning',
      title: 'Fleet UI on hold · bot updates stopped',
      detail: '1 of 4 bots updated before you stopped. The rest stay on their current version.',
    })
  })
})

describe('outcomeView', () => {
  it('shows nothing without a note', () => {
    expect(outcomeView({ phase: 'none' })).toBeNull()
  })

  it('ticks every step and names both versions', () => {
    const view = outcomeView({ phase: 'updated', from: '0.1.400', version: '0.1.405', bots: 4, failed: 0 })
    expect(view).toMatchObject({
      tone: 'success',
      title: 'Fleet UI and 4 bots updated',
      step: 4,
      dismissible: true,
    })
    expect(view?.detail).toMatch(/^Fleet UI is now on v0\.1\.405 \(was v0\.1\.400\)\./)
    expect(outcomeView({ phase: 'updated', from: 'a', version: 'b', bots: 0, failed: 0 })).toMatchObject({
      title: 'Fleet UI updated',
      step: 3,
    })
  })

  it("doesn't claim every bot came back when Fleet UI was updated anyway", () => {
    const view = outcomeView({ phase: 'updated', from: 'a', version: 'b', bots: 4, failed: 1 })
    expect(view).toMatchObject({ tone: 'warning', title: 'Fleet UI and 4 bots updated', actions: ['fleet'] })
    expect(view?.detail).toMatch(/1 bot didn't come back up/)
    expect(view?.detail).not.toMatch(/all came back/)
  })

  it("warns when Fleet UI came back on the same version", () => {
    expect(outcomeView({ phase: 'not-moved', bots: 2 })).toMatchObject({
      tone: 'warning',
      title: "Fleet UI didn't update",
    })
  })
})

describe('afterPanelView (from an older page)', () => {
  const at = { from: '0.1.399', version: '0.1.405' }

  it('shows nothing without a handover', () => {
    expect(afterPanelView({ phase: 'none' }, null)).toBeNull()
  })

  it('keeps the older order: Fleet UI first, bots on step four', () => {
    const view = afterPanelView({ phase: 'checking', ...at, bots: 2 }, null)
    expect(view).toMatchObject({ tone: 'progress', step: 3, title: 'Fleet UI updated' })
    expect(view?.steps).toEqual(['Download', 'Restart', 'Reconnect', 'Update bots'])
  })

  it('counts the run as it goes and ends on its outcome', () => {
    const going = setStep(startRun(BOT_TARGET, [candidate('a'), candidate('b')]), 'a', 'done')
    expect(afterPanelView({ phase: 'bots', ...at }, going)?.title).toBe(
      'Fleet UI updated · bots 1 of 2',
    )
    expect(afterPanelView({ phase: 'bots', ...at }, ended(['done']))).toMatchObject({
      tone: 'success',
      title: 'Fleet UI and 1 bot updated',
      step: 4,
    })
    expect(afterPanelView({ phase: 'bots', ...at }, ended(['done', 'failed']))?.title).toBe(
      "Fleet UI updated · 1 of 2 bots didn't come back",
    )
  })

  it('explains each reason the bots were left alone', () => {
    const skips: [AfterPanel, string, string][] = [
      [{ phase: 'skipped', ...at, reason: 'panel-not-moved' }, 'warning', "Fleet UI didn't update"],
      [{ phase: 'skipped', ...at, reason: 'registry' }, 'warning', 'Fleet UI updated, bots not checked'],
      [{ phase: 'skipped', ...at, reason: 'check-failed', error: 'boom' }, 'warning', 'Fleet UI updated, bots not checked'],
      [{ phase: 'skipped', ...at, reason: 'expired' }, 'warning', 'Bot updates not started'],
      [{ phase: 'skipped', ...at, reason: 'nothing-behind' }, 'success', 'Fleet UI updated'],
    ]
    for (const [state, tone, title] of skips) {
      expect(afterPanelView(state, null)).toMatchObject({ tone, title, dismissible: true })
    }
  })
})

describe('panelUpdateBusy', () => {
  it('holds the button only while an update is under way', () => {
    const cases: [PanelUpdate, boolean][] = [
      [{ phase: 'idle' }, false],
      [{ phase: 'pulling', ...alone }, true],
      [{ phase: 'restarting', ...afterBots }, true],
      [{ phase: 'slow', ...alone }, false],
      [{ phase: 'failed', error: 'x', ...alone }, false],
    ]
    expect(cases.map(([u]) => panelUpdateBusy(u))).toEqual(cases.map(([, b]) => b))
  })
})

describe('panelFlowBusy', () => {
  it('also holds Update all while bots go first or the older handover decides', () => {
    const idle: PanelUpdate = { phase: 'idle' }
    const none: AfterPanel = { phase: 'none' }
    const quiet: FleetUpgrade = { phase: 'idle' }
    expect(panelFlowBusy(idle, none, quiet)).toBe(false)
    expect(panelFlowBusy(idle, none, { phase: 'bots', from: 'a', signsOut: false })).toBe(true)
    expect(panelFlowBusy(idle, { phase: 'checking', from: 'a', version: 'b', bots: 1 }, quiet)).toBe(true)
    expect(panelFlowBusy({ phase: 'armed', ...alone }, none, quiet)).toBe(true)
  })
})

describe('fleetNow', () => {
  it('says what Fleet UI moved from, or just where it is when it held', () => {
    expect(fleetNow('0.1.233', '0.1.240')).toBe('Fleet UI is now on v0.1.240 (was v0.1.233).')
    expect(fleetNow('0.1.240', '0.1.240')).toBe('Fleet UI is now on v0.1.240.')
    expect(fleetNow('', '0.1.240')).toBe('Fleet UI is now on v0.1.240.')
  })
})

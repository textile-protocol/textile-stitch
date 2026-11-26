import { describe, expect, it } from 'vitest'
import { finishRun, setStep, startRun, type UpdateCandidate } from './fleetUpdate'
import {
  afterPanelView,
  panelFlowBusy,
  panelUpdateBusy,
  panelUpdateView,
  fleetNow,
  type AfterPanel,
  type PanelUpdate,
} from './panelUpdate'

const BOT_TARGET = 'ghcr.io/textile-protocol/textile-stitch:latest'

function candidate(name: string): UpdateCandidate {
  return { name, label: name, image: null, version: null, wantsUp: true }
}

describe('panelUpdateView', () => {
  it('shows nothing when no update is running', () => {
    expect(panelUpdateView({ phase: 'idle' })).toBeNull()
  })

  it('walks the three steps in order', () => {
    const views = (['pulling', 'armed', 'restarting', 'reloading'] as const).map((phase) =>
      panelUpdateView({ phase, bots: 0 }),
    )
    expect(views.map((v) => v?.step)).toEqual([0, 1, 1, 2])
    expect(views[0]?.steps).toEqual(['Download', 'Restart', 'Reconnect'])
  })

  it('adds an Update bots step and says how many follow when bots are ticked', () => {
    const view = panelUpdateView({ phase: 'pulling', bots: 3 })
    expect(view?.steps).toEqual(['Download', 'Restart', 'Reconnect', 'Update bots'])
    expect(view?.title).toBe('Updating Fleet and 3 bots')
    expect(view?.detail).toMatch(/update once Fleet is back/)
    expect(panelUpdateView({ phase: 'pulling', bots: 1 })?.title).toBe('Updating Fleet and 1 bot')
  })

  it('talks about Fleet and trading, never the panel or images', () => {
    const phases = ['pulling', 'armed', 'restarting', 'reloading', 'slow'] as const
    const text = phases
      .map((phase) => panelUpdateView({ phase, bots: 2 }))
      .map((v) => `${v?.title} ${v?.detail}`)
      .join(' ')
    expect(text).not.toMatch(/panel|image|container|registry|pull/i)
  })

  it("shows the server's error verbatim, without steps, and says no bots moved", () => {
    expect(panelUpdateView({ phase: 'failed', error: 'no container', bots: 0 })).toMatchObject({
      tone: 'danger',
      detail: 'no container',
      step: null,
      dismissible: true,
    })
    expect(panelUpdateView({ phase: 'failed', error: 'no container', bots: 2 })?.detail).toBe(
      "no container Your bots weren't touched.",
    )
  })

  it('switches to a warning with Reload when the panel never came back', () => {
    expect(panelUpdateView({ phase: 'slow', bots: 0 })).toMatchObject({
      tone: 'warning',
      step: null,
      action: 'reload',
    })
  })
})

describe('afterPanelView', () => {
  const at = { from: '0.1.233', version: '0.1.240' }

  it('shows nothing without a handover', () => {
    expect(afterPanelView({ phase: 'none' }, null)).toBeNull()
  })

  it('picks up on the fourth step while it checks', () => {
    const view = afterPanelView({ phase: 'checking', ...at, bots: 2 }, null)
    expect(view).toMatchObject({ tone: 'progress', step: 3, title: 'Fleet updated' })
    expect(view?.detail).toMatch(/^Fleet is now on v0\.1\.240 \(was v0\.1\.233\)\./)
  })

  it('counts the run as it goes and links to Fleet', () => {
    const run = setStep(startRun(BOT_TARGET, [candidate('a'), candidate('b')]), 'a', 'done')
    const view = afterPanelView({ phase: 'bots', ...at }, run)
    expect(view).toMatchObject({
      tone: 'progress',
      title: 'Fleet updated · bots 1 of 2',
      step: 3,
      action: 'fleet',
      dismissible: false,
    })
  })

  it('ticks every step when all bots came back', () => {
    const run = finishRun(setStep(startRun(BOT_TARGET, [candidate('a')]), 'a', 'done'), 'done')
    expect(afterPanelView({ phase: 'bots', ...at }, run)).toMatchObject({
      tone: 'success',
      title: 'Fleet and 1 bot updated',
      step: 4,
      dismissible: true,
    })
  })

  it('turns danger and points at Fleet when a bot failed', () => {
    let run = startRun(BOT_TARGET, [candidate('a'), candidate('b')])
    run = finishRun(setStep(setStep(run, 'a', 'done'), 'b', 'failed', 'exited'), 'done')
    const away = afterPanelView({ phase: 'bots', ...at }, run)
    expect(away).toMatchObject({
      tone: 'danger',
      title: "Fleet updated · 1 of 2 bots didn't come back",
      action: 'fleet',
    })
    expect(away?.detail).toMatch(/View your bots to see which one/)
    // On Fleet the card is right there, so the strip points at it instead.
    expect(afterPanelView({ phase: 'bots', ...at }, run, true)?.detail).toMatch(
      /Open the one that failed, below/,
    )
  })

  it('says how far a stopped run got', () => {
    let run = startRun(BOT_TARGET, [candidate('a'), candidate('b')])
    run = finishRun(setStep(run, 'a', 'done'), 'cancelled')
    const view = afterPanelView({ phase: 'bots', ...at }, run)
    expect(view).toMatchObject({ tone: 'warning', title: 'Fleet updated · bot updates stopped' })
    expect(view?.detail).toMatch(/^1 of 2 bots updated/)
  })

  it('hides once the run card on Fleet was dismissed', () => {
    expect(afterPanelView({ phase: 'bots', ...at }, null)).toBeNull()
  })

  it('explains each reason the bots were left alone', () => {
    const skips: [AfterPanel, string, string][] = [
      [{ phase: 'skipped', ...at, reason: 'panel-not-moved' }, 'warning', "Fleet didn't update"],
      [{ phase: 'skipped', ...at, reason: 'registry' }, 'warning', 'Fleet updated, bots not checked'],
      [{ phase: 'skipped', ...at, reason: 'check-failed', error: 'boom' }, 'warning', 'Fleet updated, bots not checked'],
      [{ phase: 'skipped', ...at, reason: 'expired' }, 'warning', 'Bot updates not started'],
      [{ phase: 'skipped', ...at, reason: 'nothing-behind' }, 'success', 'Fleet updated'],
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
      [{ phase: 'pulling', bots: 0 }, true],
      [{ phase: 'restarting', bots: 2 }, true],
      [{ phase: 'slow', bots: 0 }, false],
      [{ phase: 'failed', error: 'x', bots: 0 }, false],
    ]
    expect(cases.map(([u]) => panelUpdateBusy(u))).toEqual(cases.map(([, b]) => b))
  })
})

describe('panelFlowBusy', () => {
  it('also holds Update all while the handover decides', () => {
    const idle: PanelUpdate = { phase: 'idle' }
    expect(panelFlowBusy(idle, { phase: 'none' })).toBe(false)
    expect(panelFlowBusy(idle, { phase: 'checking', from: 'a', version: 'b', bots: 1 })).toBe(true)
    expect(panelFlowBusy(idle, { phase: 'bots', from: 'a', version: 'b' })).toBe(false)
    expect(panelFlowBusy({ phase: 'armed', bots: 0 }, { phase: 'none' })).toBe(true)
  })
})

describe('fleetNow', () => {
  it('says what Fleet moved from, or just where it is when it held', () => {
    expect(fleetNow('0.1.233', '0.1.240')).toBe('Fleet is now on v0.1.240 (was v0.1.233).')
    expect(fleetNow('0.1.240', '0.1.240')).toBe('Fleet is now on v0.1.240.')
    expect(fleetNow('', '0.1.240')).toBe('Fleet is now on v0.1.240.')
  })
})

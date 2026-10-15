import { describe, expect, it } from 'vitest'
import { panelUpdateBusy, panelUpdateView, type PanelUpdate } from './panelUpdate'

const TARGET = 'ghcr.io/textile-protocol/textile-stitch-panel:latest'

describe('panelUpdateView', () => {
  it('shows nothing when no update is running', () => {
    expect(panelUpdateView({ phase: 'idle' })).toBeNull()
  })

  it('walks the three steps in order', () => {
    const steps = (['pulling', 'armed', 'restarting', 'reloading'] as const).map(
      (phase) => panelUpdateView({ phase, target: TARGET })?.step,
    )
    expect(steps).toEqual([0, 1, 1, 2])
  })

  it('names the target by its short ref, not the full registry path', () => {
    expect(panelUpdateView({ phase: 'pulling', target: TARGET })?.target).toBe(
      'textile-stitch-panel:latest',
    )
    expect(panelUpdateView({ phase: 'pulling', target: null })?.target).toBeNull()
  })

  it("shows the server's error verbatim, without steps", () => {
    const view = panelUpdateView({ phase: 'failed', error: 'no container' })
    expect(view).toMatchObject({ tone: 'danger', detail: 'no container', step: null })
  })

  it('switches to a warning when the panel never came back', () => {
    expect(panelUpdateView({ phase: 'slow', target: TARGET })).toMatchObject({
      tone: 'warning',
      step: null,
    })
  })
})

describe('panelUpdateBusy', () => {
  it('holds the button only while an update is under way', () => {
    const cases: [PanelUpdate, boolean][] = [
      [{ phase: 'idle' }, false],
      [{ phase: 'pulling', target: null }, true],
      [{ phase: 'restarting', target: null }, true],
      [{ phase: 'slow', target: null }, false],
      [{ phase: 'failed', error: 'x' }, false],
    ]
    expect(cases.map(([u]) => panelUpdateBusy(u))).toEqual(cases.map(([, b]) => b))
  })
})

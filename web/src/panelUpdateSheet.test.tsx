import { createElement as h } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'
import PanelUpdateSheet, { type BotsSection } from './components/PanelUpdateSheet'
import type { FleetUpdatePlan } from './fleetUpdate'

const PANEL = 'ghcr.io/textile-protocol/textile-stitch-panel:latest'

const plan: FleetUpdatePlan = {
  ready: [
    { name: 'a', label: 'Lagos', image: null, version: 'v0.1.233', wantsUp: true },
    { name: 'b', label: 'Accra', image: null, version: 'v0.1.233', wantsUp: false },
  ],
  skipped: [{ name: 'c', label: 'Joburg', reason: 'Pinned to a build.' }],
}

function render(bots: BotsSection | null, signsOut = false): string {
  return renderToStaticMarkup(
    h(PanelUpdateSheet, {
      open: true,
      panelTarget: PANEL,
      version: '0.1.233',
      bots,
      signsOut,
      onClose: () => {},
      onStart: () => {},
    }),
  )
}

describe('PanelUpdateSheet', () => {
  it('updates every bot by default and folds the list away', () => {
    const html = render({ state: 'ready', plan })
    expect(html).toContain('>Fleet UI<')
    expect(html).toContain('>Stitch bots<')
    expect(html).toContain('Update Fleet and 2 bots')
    expect(html).toContain('Both bots update once Fleet is back')
    expect(html).toContain('aria-checked="true"')
    // The per-bot list is there, inside a <details> that starts closed.
    expect(html).toMatch(/<details(?![^>]*\sopen)[^>]*>.*Choose bots.*2 of 2 selected/s)
    expect(html).toContain('Not updated (1)')
  })

  it('never says panel, image or container to the operator', () => {
    const html = render({ state: 'ready', plan }).replace(/<[^>]*>/g, ' ')
    expect(html).not.toMatch(/panel|image|container|registry/i)
  })

  it('offers Fleet alone when bulk bot updates are off', () => {
    const html = render(null)
    expect(html).toContain('>Update Fleet<')
    expect(html).not.toContain('Stitch bots')
  })

  it("holds the button until it knows what's behind", () => {
    const html = render({ state: 'loading' })
    expect(html).toContain('Checking which bots need an update')
    expect(html).toMatch(/<button[^>]*disabled=""[^>]*>Update Fleet</)
  })

  it('warns a password session that it signs back in', () => {
    expect(render({ state: 'ready', plan }, true)).toContain('sign in again after the restart')
  })
})

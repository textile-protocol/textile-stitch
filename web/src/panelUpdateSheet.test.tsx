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
  it('puts the bots first, every one on by default, with the list folded away', () => {
    const html = render({ state: 'ready', plan })
    // Cards in the order things happen.
    expect(html.indexOf('>Stitch bots<')).toBeLessThan(html.indexOf('>Fleet UI<'))
    expect(html).toContain('Your bots first, then Fleet UI.')
    expect(html).toContain('Update 2 bots and Fleet UI')
    expect(html).toContain('Both bots update one at a time')
    expect(html).toContain('aria-checked="true"')
    // The per-bot list is there, inside a <details> that starts closed.
    expect(html).toMatch(/<details(?![^>]*\sopen)[^>]*>.*Choose bots.*2 of 2 selected/s)
    expect(html).toContain('Not updated (1)')
  })

  it("says Fleet UI takes seconds and waits for a bot that didn't come back", () => {
    const html = render({ state: 'ready', plan })
    expect(html).toContain('Updates once your bots are done. Takes a few seconds')
    expect(html).not.toMatch(/minute/)
    expect(html).toContain('Fleet UI waits so you can fix it first')
  })

  it('never says panel, image or container to the operator', () => {
    const html = render({ state: 'ready', plan }).replace(/<[^>]*>/g, ' ')
    expect(html).not.toMatch(/panel|image|container|registry/i)
  })

  it('offers Fleet UI alone when bulk bot updates are off', () => {
    const html = render(null)
    expect(html).toContain('>Update Fleet UI<')
    expect(html).toContain('Takes a few seconds.')
    expect(html).not.toContain('Stitch bots')
  })

  it("holds the button until it knows what's behind", () => {
    const html = render({ state: 'loading' })
    expect(html).toContain('Checking which bots need an update')
    expect(html).toMatch(/<button[^>]*disabled=""[^>]*>Update Fleet UI</)
  })

  it('tells a password session it signs in again at the end', () => {
    expect(render({ state: 'ready', plan }, true)).toContain('sign in again at the end')
  })
})

import { describe, expect, it } from 'vitest'
import {
  HANDOFF_TTL_MS,
  decodeHandoff,
  encodeHandoff,
  planAfterPanel,
  type BotsAfterPanel,
} from './panelHandoff'
import type { Bot, UpdatesStatus } from './types'

const LATEST = 'ghcr.io/textile-protocol/textile-stitch:latest'
const PIN = 'ghcr.io/textile-protocol/textile-stitch:sha-14cd877'
const NOW = 1_800_000_000_000

const note = (bots: string[], at = NOW): BotsAfterPanel => ({ bots, from: '0.1.233', at })

// Only the fields the planner reads.
function bot(name: string, opts: Partial<Bot> = {}): Bot {
  return {
    name,
    displayName: null,
    layout: 'per-bot-dir',
    container: `stitch-${name}`,
    state: 'running',
    image: LATEST,
    version: 'v0.1.233',
    editable: true,
    canMigrate: false,
    warnings: [],
    ...opts,
  } as Bot
}

function updates(
  behind: string[],
  opts: { panelBehind?: boolean; registryError?: string } = {},
): UpdatesStatus {
  const info = { targetImage: LATEST, currentImage: LATEST, reason: null }
  return {
    bot: { ...info, updateAvailable: behind.length > 0, reason: opts.registryError ?? null },
    panel: { ...info, updateAvailable: opts.panelBehind ?? false },
    bots: behind.map((name) => ({
      name,
      currentImage: LATEST,
      updateAvailable: true,
      canUpdate: true,
    })),
  }
}

describe('decodeHandoff', () => {
  it('reads back what encodeHandoff wrote', () => {
    const h = note(['a', 'b'])
    expect(decodeHandoff(encodeHandoff(h), NOW + 5_000)).toEqual({ kind: 'ready', handoff: h })
  })

  it('treats no value, bad JSON and other shapes as no note', () => {
    const bad = [
      null,
      '',
      'not json',
      '[]',
      JSON.stringify({ v: 2, bots: ['a'], from: 'x', at: NOW }),
      JSON.stringify({ v: 1, bots: [], from: 'x', at: NOW }),
      JSON.stringify({ v: 1, bots: ['a', 7], from: 'x', at: NOW }),
      JSON.stringify({ v: 1, bots: [''], from: 'x', at: NOW }),
      JSON.stringify({ v: 1, bots: ['a'], from: 'x', at: 'soon' }),
      JSON.stringify({ v: 1, bots: ['a'], at: NOW }),
      JSON.stringify({ v: 1, bots: Array.from({ length: 501 }, (_, i) => `b${i}`), from: 'x', at: NOW }),
    ]
    expect(bad.map((raw) => decodeHandoff(raw, NOW).kind)).toEqual(bad.map(() => 'none'))
  })

  it('refuses a note dated in the future', () => {
    expect(decodeHandoff(encodeHandoff(note(['a'], NOW + 10 * 60_000)), NOW).kind).toBe('none')
  })

  it('goes stale after the TTL', () => {
    const raw = encodeHandoff(note(['a']))
    expect(decodeHandoff(raw, NOW + HANDOFF_TTL_MS).kind).toBe('ready')
    expect(decodeHandoff(raw, NOW + HANDOFF_TTL_MS + 1).kind).toBe('expired')
  })

  it('drops duplicate names', () => {
    const raw = JSON.stringify({ v: 1, bots: ['a', 'b', 'a'], from: 'x', at: NOW })
    const read = decodeHandoff(raw, NOW)
    expect(read.kind === 'ready' && read.handoff.bots).toEqual(['a', 'b'])
  })
})

describe('planAfterPanel', () => {
  it("runs the note's bots in the note's order", () => {
    const plan = planAfterPanel(note(['c', 'a']), [bot('a'), bot('b'), bot('c')], updates(['a', 'b', 'c']))
    expect(plan.kind === 'run' && plan.picked.map((c) => c.name)).toEqual(['c', 'a'])
    expect(plan.kind === 'run' && plan.target).toBe(LATEST)
  })

  it("never adds a bot the operator didn't tick", () => {
    const plan = planAfterPanel(note(['a']), [bot('a'), bot('b')], updates(['a', 'b']))
    expect(plan.kind === 'run' && plan.picked.map((c) => c.name)).toEqual(['a'])
  })

  it('leaves out ticked bots that are current, gone, pinned or on the flat layout', () => {
    const fleet = [
      bot('current'),
      bot('pinned', { image: PIN }),
      bot('flat', { layout: 'flat-files', canMigrate: true }),
      bot('ok'),
    ]
    const plan = planAfterPanel(
      note(['current', 'gone', 'pinned', 'flat', 'ok']),
      fleet,
      updates(['pinned', 'flat', 'ok']),
    )
    expect(plan.kind === 'run' && plan.picked.map((c) => c.name)).toEqual(['ok'])
  })

  it("doesn't touch bots when the panel still reports itself behind", () => {
    const plan = planAfterPanel(note(['a']), [bot('a')], updates(['a'], { panelBehind: true }))
    expect(plan).toEqual({ kind: 'skip', reason: 'panel-not-moved' })
  })

  it('says the registry is why nothing reads as behind', () => {
    const plan = planAfterPanel(note(['a']), [bot('a')], updates([], { registryError: 'timeout' }))
    expect(plan).toEqual({ kind: 'skip', reason: 'registry' })
  })

  it('has nothing to do when every ticked bot is already current', () => {
    expect(planAfterPanel(note(['a']), [bot('a')], updates([]))).toEqual({
      kind: 'skip',
      reason: 'nothing-behind',
    })
  })
})

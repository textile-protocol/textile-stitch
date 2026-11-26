import { describe, expect, it } from 'vitest'
import { OUTCOME_TTL_MS, decodeOutcome, encodeOutcome } from './updateOutcome'

const NOW = 1_800_000_000_000

describe('decodeOutcome', () => {
  it('reads back what encodeOutcome wrote', () => {
    const o = { from: '0.1.400', bots: 3, failed: 1, at: NOW }
    expect(decodeOutcome(encodeOutcome(o), NOW + 5_000)).toEqual(o)
  })

  it('treats no value, bad JSON and other shapes as no note', () => {
    const bad = [
      null,
      '',
      'not json',
      '[]',
      JSON.stringify({ v: 2, from: 'x', bots: 1, at: NOW }),
      JSON.stringify({ v: 1, from: 'x', bots: -1, failed: 0, at: NOW }),
      JSON.stringify({ v: 1, from: 'x', bots: 1, at: NOW }),
      JSON.stringify({ v: 1, from: 'x', bots: 1.5, at: NOW }),
      JSON.stringify({ v: 1, bots: 1, at: NOW }),
      JSON.stringify({ v: 1, from: 'x', bots: 1, at: 'soon' }),
    ]
    expect(bad.map((raw) => decodeOutcome(raw, NOW))).toEqual(bad.map(() => null))
  })

  it('drops a note dated in the future or older than the TTL', () => {
    expect(decodeOutcome(encodeOutcome({ from: 'x', bots: 0, failed: 0, at: NOW + 10 * 60_000 }), NOW)).toBeNull()
    const raw = encodeOutcome({ from: 'x', bots: 0, failed: 0, at: NOW })
    expect(decodeOutcome(raw, NOW + OUTCOME_TTL_MS)).not.toBeNull()
    expect(decodeOutcome(raw, NOW + OUTCOME_TTL_MS + 1)).toBeNull()
  })
})

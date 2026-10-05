import { describe, expect, it } from 'vitest'
import {
  equityCoordinates,
  moduleStatusFresh,
  type ModuleStatus,
} from './modules'

describe('module telemetry', () => {
  const status = {
    at: 100,
    decisions: [{ decision: { at: 100 } }],
  } as ModuleStatus
  it('does not label old, future or stopped telemetry as current', () => {
    expect(moduleStatusFresh(status, true, 101)).toBe(true)
    expect(moduleStatusFresh(status, true, 111)).toBe(false)
    expect(moduleStatusFresh(status, true, 99)).toBe(false)
    expect(moduleStatusFresh(status, false, 101)).toBe(false)
    expect(moduleStatusFresh(null, true, 101)).toBe(false)
  })
  it('plots differences above Number.MAX_SAFE_INTEGER without rounding away the result', () => {
    const n = 10n ** 30n
    const points = [
      { at: 1, baseline: n.toString(), candidate: n.toString() },
      { at: 2, baseline: n.toString(), candidate: (n + 1n).toString() },
    ]
    expect(equityCoordinates(points, 'candidate')).toBe('20,180 680,30')
    expect(equityCoordinates(points, 'baseline')).toBe('20,180 680,180')
    expect(equityCoordinates([], 'baseline')).toBe('')
  })
})

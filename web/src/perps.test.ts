import { describe, expect, it } from 'vitest'
import type { PerpsView } from './api'
import {
  allocationAtoms,
  allocationTokens,
  hedgeMode,
  hedgeSnapshotCurrent,
  hedgeStatus,
  showHedgeModule,
} from './perps'

const now = Date.parse('2026-10-28T19:00:00Z')
const empty = (): PerpsView => ({
  enabled: false,
  recallEnabled: false,
  assignmentId: 'idrx-fleet-production-1',
  fundingTargetAtoms: '0',
  unavailable: false,
  remote: {
    control: { mode: 'off' },
    effectiveMode: 'shadow',
    monitoring: {
      hasFunds: false,
      observedAt: new Date(now).toISOString(),
      pendingQuotes: 0,
      pendingActions: 0,
      inventoryOperations: [],
    },
  },
})

describe('hedge module visibility', () => {
  it('shows Preview without funding and hides Off only after a current empty read', () => {
    const view = empty()
    expect(showHedgeModule(view, now)).toBe(false)
    view.enabled = true
    view.remote!.control!.mode = 'preview'
    expect(showHedgeModule(view, now)).toBe(true)
    expect(hedgeMode(view)).toBe('preview')
  })
  it.each(['hasFunds', 'pendingQuotes', 'pendingActions'] as const)(
    'retains an Off module with %s',
    (key) => {
      const view = empty()
      Object.assign(view.remote!.monitoring!, {
        [key]: key === 'hasFunds' ? true : 21,
      })
      expect(showHedgeModule(view, now)).toBe(true)
    }
  )
  it('retains monitoring through an outage or an older API rollout', () => {
    const view = empty()
    view.unavailable = true
    expect(showHedgeModule(view, now)).toBe(true)
    view.unavailable = false
    delete view.remote!.monitoring
    expect(showHedgeModule(view, now)).toBe(true)
  })
  it.each([null, 'not a date', '2026-10-28T18:59:44Z', '2026-10-28T19:00:01Z'])(
    'does not treat %s as a current snapshot',
    (at) => {
      const view = empty()
      view.remote!.monitoring!.observedAt = at
      expect(hedgeSnapshotCurrent(view, now)).toBe(false)
      expect(showHedgeModule(view, now)).toBe(true)
    }
  )
  it('keeps local Off authoritative when the remote service still reports Live', () => {
    const view = empty()
    view.remote!.control!.mode = 'live'
    expect(hedgeMode(view)).toBe('off')
    view.enabled = true
    view.remote = null
    expect(hedgeMode(view)).toBe('unknown')
  })
  it('does not expose an unused module for an unconfigured bot', () => {
    expect(showHedgeModule(null, now)).toBe(false)
    expect(showHedgeModule({ ...empty(), assignmentId: '' }, now)).toBe(false)
  })
  it('distinguishes a saved Live choice from permission to execute', () => {
    const view = empty()
    view.enabled = true
    view.remote!.control!.mode = 'live'
    expect(hedgeStatus(view, now)).toBe('Execution disabled')
    view.recallEnabled = true
    expect(hedgeStatus(view, now)).toBe('Awaiting authorization')
    view.remote!.effectiveMode = 'live'
    expect(hedgeStatus(view, now)).toBe('Live')
    expect(hedgeStatus(view, now + 16000)).toBe('Waiting for snapshot')
    expect(hedgeStatus(view, now, true)).toBe('Unavailable')
  })
})

describe('operator allocation inputs', () => {
  it('round trips IDRX, whole tokens and 18-decimal values without float loss', () => {
    expect(allocationAtoms('250000.01', 2)).toBe('25000001')
    expect(allocationAtoms('0', 2)).toBe('0')
    expect(allocationTokens('25000001', 2)).toBe('250000.01')
    expect(allocationTokens('25', 0)).toBe('25')
    const raw = '99999999999999999999999999999999999999'
    expect(allocationAtoms(allocationTokens(raw, 18), 18)).toBe(raw)
  })
  it.each(['1.001', '-1', '1e6', 'NaN', '', '2,500', 'Infinity'])(
    'rejects %s instead of rounding or increasing authorization',
    (input) => {
      expect(allocationAtoms(input, 2)).toBeNull()
    }
  )
})

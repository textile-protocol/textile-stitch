import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  MASK,
  balancesHidden,
  maskBalance,
  readBalancesHidden,
  saveBalancesHidden,
  setBalancesHidden,
} from './balancePrivacy'

describe('balance privacy', () => {
  const stubStorage = (raw: string | null) => {
    const store = new Map(raw === null ? [] : [['stitch-panel-hide-balances', raw]])
    vi.stubGlobal('window', {
      localStorage: {
        getItem: (k: string) => store.get(k) ?? null,
        setItem: (k: string, v: string) => void store.set(k, v),
      },
      addEventListener: () => {},
    })
  }

  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('round-trips both choices', () => {
    stubStorage(null)
    saveBalancesHidden(true)
    expect(readBalancesHidden()).toBe(true)
    saveBalancesHidden(false)
    expect(readBalancesHidden()).toBe(false)
  })

  it.each([
    ['nothing saved', null],
    ['an unknown value', 'yes'],
  ])('reads %s as shown', (_, raw) => {
    stubStorage(raw)
    expect(readBalancesHidden()).toBe(false)
  })

  it('reads as shown when storage throws', () => {
    vi.stubGlobal('window', {
      localStorage: {
        getItem: () => {
          throw new Error('denied')
        },
      },
    })
    expect(readBalancesHidden()).toBe(false)
  })

  it('swallows a failed save', () => {
    vi.stubGlobal('window', {
      localStorage: {
        setItem: () => {
          throw new Error('quota')
        },
      },
    })
    expect(() => saveBalancesHidden(true)).not.toThrow()
  })

  it('keeps the flag in memory and saves it', () => {
    stubStorage(null)
    setBalancesHidden(true)
    expect(balancesHidden()).toBe(true)
    expect(readBalancesHidden()).toBe(true)
    setBalancesHidden(false)
    expect(balancesHidden()).toBe(false)
  })

  it('still flips when storage refuses the write', () => {
    vi.stubGlobal('window', {
      localStorage: {
        setItem: () => {
          throw new Error('quota')
        },
      },
      addEventListener: () => {},
    })
    setBalancesHidden(true)
    expect(balancesHidden()).toBe(true)
    setBalancesHidden(false)
  })
})

describe('maskBalance', () => {
  it('masks with a fixed length whatever the amount', () => {
    expect(maskBalance('1.5', true)).toBe(MASK)
    expect(maskBalance('1,250,000.75', true)).toBe(MASK)
  })

  it('passes the amount through when shown', () => {
    expect(maskBalance('1.5', false)).toBe('1.5')
  })
})

describe('cross-tab sync', () => {
  afterEach(() => {
    vi.unstubAllGlobals()
    vi.resetModules()
  })

  it('picks up a change from another tab with nothing subscribed', async () => {
    let handler: ((e: StorageEvent) => void) | null = null
    vi.stubGlobal('window', {
      localStorage: { getItem: () => null, setItem: () => {} },
      addEventListener: (type: string, h: (e: StorageEvent) => void) => {
        if (type === 'storage') handler = h
      },
    })
    vi.resetModules()
    const mod = await import('./balancePrivacy')
    expect(mod.balancesHidden()).toBe(false)
    handler!({ key: 'stitch-panel-hide-balances', newValue: '1' } as StorageEvent)
    expect(mod.balancesHidden()).toBe(true)
    handler!({ key: 'something-else', newValue: '0' } as StorageEvent)
    expect(mod.balancesHidden()).toBe(true)
  })
})

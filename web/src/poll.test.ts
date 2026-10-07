import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { poll, type Visibility } from './poll'

/** A page whose visibility the test flips by hand. */
function fakePage(hidden = false) {
  let isHidden = hidden
  const listeners = new Set<() => void>()
  const visibility: Visibility = {
    hidden: () => isHidden,
    subscribe: (l) => {
      listeners.add(l)
      return () => listeners.delete(l)
    },
  }
  const set = (next: boolean) => {
    isHidden = next
    listeners.forEach((l) => l())
  }
  return {
    visibility,
    hide: () => set(true),
    show: () => set(false),
    listeners,
  }
}

describe('poll', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  it('reads at once when asked to, then every interval', async () => {
    const page = fakePage()
    const tick = vi.fn(() => Promise.resolve())
    const stop = poll(tick, 5000, {
      immediate: true,
      visibility: page.visibility,
    })
    expect(tick).toHaveBeenCalledTimes(1)
    await vi.advanceTimersByTimeAsync(5000)
    expect(tick).toHaveBeenCalledTimes(2)
    await vi.advanceTimersByTimeAsync(5000)
    expect(tick).toHaveBeenCalledTimes(3)
    stop()
  })

  it('waits a full interval first without immediate', async () => {
    const page = fakePage()
    const tick = vi.fn(() => Promise.resolve())
    const stop = poll(tick, 5000, { visibility: page.visibility })
    expect(tick).not.toHaveBeenCalled()
    await vi.advanceTimersByTimeAsync(5000)
    expect(tick).toHaveBeenCalledTimes(1)
    stop()
  })

  it('never sends a read while the last one is still out', async () => {
    const page = fakePage()
    let settle = () => {}
    const tick = vi.fn(() => new Promise<void>((r) => (settle = r)))
    const stop = poll(tick, 5000, {
      immediate: true,
      visibility: page.visibility,
    })
    // A node taking 20s: no reads pile up behind it.
    await vi.advanceTimersByTimeAsync(20_000)
    expect(tick).toHaveBeenCalledTimes(1)
    settle()
    // The next one is an interval after this one settled.
    await vi.advanceTimersByTimeAsync(4999)
    expect(tick).toHaveBeenCalledTimes(1)
    await vi.advanceTimersByTimeAsync(1)
    expect(tick).toHaveBeenCalledTimes(2)
    stop()
  })

  it('stops reading while hidden and reads at once on return', async () => {
    const page = fakePage()
    const tick = vi.fn(() => Promise.resolve())
    const stop = poll(tick, 5000, {
      immediate: true,
      visibility: page.visibility,
    })
    await vi.advanceTimersByTimeAsync(0)
    page.hide()
    await vi.advanceTimersByTimeAsync(60_000)
    expect(tick).toHaveBeenCalledTimes(1)
    page.show()
    expect(tick).toHaveBeenCalledTimes(2)
    await vi.advanceTimersByTimeAsync(5000)
    expect(tick).toHaveBeenCalledTimes(3)
    stop()
  })

  it('a quick hide and show does not read twice', async () => {
    const page = fakePage()
    const tick = vi.fn(() => Promise.resolve())
    const stop = poll(tick, 5000, {
      immediate: true,
      visibility: page.visibility,
    })
    await vi.advanceTimersByTimeAsync(1000)
    page.hide()
    page.show()
    // The pending timer still stands; showing again didn't add a read.
    expect(tick).toHaveBeenCalledTimes(1)
    await vi.advanceTimersByTimeAsync(4000)
    expect(tick).toHaveBeenCalledTimes(2)
    stop()
  })

  it('keeps going after a failed read', async () => {
    const page = fakePage()
    const tick = vi.fn(() => Promise.reject(new Error('node down')))
    const stop = poll(tick, 5000, {
      immediate: true,
      visibility: page.visibility,
    })
    await vi.advanceTimersByTimeAsync(5000)
    expect(tick).toHaveBeenCalledTimes(2)
    stop()
  })

  it('stop ends the poll and drops the listener', async () => {
    const page = fakePage()
    const tick = vi.fn(() => Promise.resolve())
    const stop = poll(tick, 5000, {
      immediate: true,
      visibility: page.visibility,
    })
    stop()
    await vi.advanceTimersByTimeAsync(20_000)
    page.show()
    expect(tick).toHaveBeenCalledTimes(1)
    expect(page.listeners.size).toBe(0)
  })

  it('starting hidden reads nothing until shown', async () => {
    const page = fakePage(true)
    const tick = vi.fn(() => Promise.resolve())
    const stop = poll(tick, 5000, {
      immediate: true,
      visibility: page.visibility,
    })
    await vi.advanceTimersByTimeAsync(30_000)
    expect(tick).not.toHaveBeenCalled()
    page.show()
    expect(tick).toHaveBeenCalledTimes(1)
    stop()
  })
})

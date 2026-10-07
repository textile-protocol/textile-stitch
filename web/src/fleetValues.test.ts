import { describe, expect, it } from 'vitest'
import { namesToRead, stillStopped } from './fleetValues'

describe('namesToRead', () => {
  it('reads running bots every round and stopped ones once', () => {
    const running = new Set(['live'])
    expect(namesToRead(['live', 'idle'], running, new Set())).toEqual(['live', 'idle'])
    expect(namesToRead(['live', 'idle'], running, new Set(['idle']))).toEqual(['live'])
  })
})

describe('stillStopped', () => {
  it('forgets a bot that started, so its next stop is read again', () => {
    const read = new Set(['a', 'b'])
    expect([...stillStopped(read, new Set(['a']))]).toEqual(['b'])
    // Stopped again: not in the set, so namesToRead picks it up.
    expect(namesToRead(['a', 'b'], new Set(), stillStopped(read, new Set(['a'])))).toEqual(['a'])
  })
})

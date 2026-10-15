import { describe, expect, it } from 'vitest'
import { createStaleGuard } from './staleGuard'

describe('createStaleGuard', () => {
  it('keeps the latest piece of work current', () => {
    const guard = createStaleGuard()
    const first = guard.begin()
    expect(first()).toBe(false)
    const second = guard.begin()
    expect(first()).toBe(true)
    expect(second()).toBe(false)
  })

  it('retires in-flight work when the inputs change', () => {
    const guard = createStaleGuard()
    const read = guard.begin()
    guard.invalidate()
    expect(read()).toBe(true)
  })

  it('retires everything once the owner unmounts', () => {
    const guard = createStaleGuard()
    const read = guard.begin()
    guard.close()
    expect(read()).toBe(true)
    // A remount (React strict mode runs effects twice) makes new work current again.
    guard.open()
    expect(guard.begin()()).toBe(false)
  })
})

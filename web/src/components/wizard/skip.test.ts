import { describe, expect, it } from 'vitest'
import { leftOver, skipDestination } from './skip'

describe('leftOver', () => {
  it('goes by the failed stage when the run failed', () => {
    expect(leftOver('approve', 'connect')).toBe('approve')
    expect(leftOver('access', 'approve')).toBe('connect')
    expect(leftOver('start', 'approve')).toBe('start')
    expect(leftOver('verify', 'approve')).toBe('start')
  })

  it("falls back to the screen's own state with no failure", () => {
    expect(leftOver(null, 'approve')).toBe('approve')
    expect(leftOver(null, 'connect')).toBe('connect')
  })
})

describe('skipDestination', () => {
  it('sends owed approvals to Tools with the Permit2 handoff', () => {
    expect(skipDestination('0xabc', 'approve')).toEqual({
      path: '/bots/0xabc?tab=tools',
      state: { needsPermit2: true },
    })
  })

  it('sends an unfinished Textile connection to Corridors, no handoff', () => {
    expect(skipDestination('my bot', 'connect')).toEqual({
      path: '/bots/my%20bot?tab=settings',
      state: null,
    })
  })

  it('sends a failed start to the bot page without the Permit2 banner', () => {
    expect(skipDestination('0xabc', 'start')).toEqual({
      path: '/bots/0xabc?tab=funds',
      state: null,
    })
  })
})

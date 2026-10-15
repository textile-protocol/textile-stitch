import { describe, expect, it } from 'vitest'
import { andList } from './format'

describe('andList', () => {
  it('joins the last item with "and"', () => {
    expect(andList([])).toBe('')
    expect(andList(['TWAP'])).toBe('TWAP')
    expect(andList(['lean', 'TWAP'])).toBe('lean and TWAP')
    expect(andList(['the taker leg', 'lean', 'TWAP'])).toBe('the taker leg, lean and TWAP')
  })
})

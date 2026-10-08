import { describe, expect, it } from 'vitest'
import { saleAtomic, saleDecimal } from './manualSales'

describe('sale amounts', () => {
  it('preserves all 18 decimals and large amounts without floating point', () => {
    const value = '270000.123456789012345678'
    expect(saleDecimal(saleAtomic(value, 18)!, 18)).toBe(value)
    expect(saleAtomic('199.60', 6)).toBe('199600000')
  })
  it('rejects rounding, scientific notation, zero and uint256 overflow', () => {
    for (const value of [
      '1.0000001',
      '1e8',
      '-1',
      '0',
      'NaN',
      (2n ** 256n).toString(),
    ])
      expect(saleAtomic(value, 6)).toBeNull()
  })
})

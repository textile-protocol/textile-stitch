import { describe, expect, it } from 'vitest'
import { absoluteUnit, formatRate, pairSymbols, sideQuote, switchUnit } from './spreadUnits'

// The feed publishes USDT per cNGN; 1530 cNGN per USDT.
const MID = 1 / 1530

describe('pairSymbols', () => {
  it('reads catalog and stamped labels', () => {
    expect(pairSymbols('cNGN / USDT')).toEqual({ collateral: 'cNGN', debt: 'USDT' })
    expect(pairSymbols('cNGN → USDT')).toEqual({ collateral: 'cNGN', debt: 'USDT' })
  })
  it('gives up on an address label', () => {
    expect(pairSymbols('0x1234…abcd / 0x5678…ef01')).toBeNull()
    expect(absoluteUnit('0x1234…abcd / 0x5678…ef01')).toBe('collateral per debt')
  })
})

describe('sideQuote', () => {
  it('moves the rate by the absolute offset on each side', () => {
    const buy = sideQuote('buy', { kind: 'abs', value: '1' }, MID, '')!
    const sell = sideQuote('sell', { kind: 'abs', value: '1' }, MID, '')!
    expect(buy.rate).toBeCloseTo(1531, 9)
    expect(sell.rate).toBeCloseTo(1529, 9)
    expect(buy.bps).toBeCloseTo(6.53, 2)
    expect(sell.bps).toBeCloseTo(6.54, 2)
    expect(buy.floored || sell.floored).toBe(false)
  })

  it('reads bps the way the bot does', () => {
    const buy = sideQuote('buy', { kind: 'bps', value: '5' }, MID, '')!
    expect(buy.bps).toBeCloseTo(5, 9)
    expect(buy.rate).toBeCloseTo(1530 / 0.9995, 6)
  })

  it('floors an absolute side once it gets thinner than the minimum', () => {
    const weak = 1 / 3000
    const quote = sideQuote('sell', { kind: 'abs', value: '1' }, weak, '5')!
    expect(quote.floored).toBe(true)
    expect(quote.bps).toBeCloseTo(5, 9)
    // At 1530 the offset is already wider than 5 bps, so it stands.
    expect(sideQuote('sell', { kind: 'abs', value: '1' }, MID, '5')!.floored).toBe(false)
  })

  it('never floors a bps side', () => {
    expect(sideQuote('buy', { kind: 'bps', value: '2' }, MID, '5')!.bps).toBeCloseTo(2, 9)
  })

  it('has nothing to show for an empty value, a bad mid or an ask past the mid', () => {
    expect(sideQuote('buy', { kind: 'abs', value: '' }, MID, '')).toBeNull()
    expect(sideQuote('buy', { kind: 'abs', value: '1' }, 0, '')).toBeNull()
    expect(sideQuote('sell', { kind: 'abs', value: '2000' }, MID, '5')).toBeNull()
  })
})

describe('switchUnit', () => {
  it('keeps the quote where it was', () => {
    const abs = switchUnit('sell', { kind: 'bps', value: '5' }, MID, '')
    expect(abs.kind).toBe('abs')
    const before = sideQuote('sell', { kind: 'bps', value: '5' }, MID, '')!
    const after = sideQuote('sell', abs, MID, '')!
    expect(after.rate).toBeCloseTo(before.rate, 2)
  })

  it('rounds bps up so the side is never tighter', () => {
    const bps = switchUnit('sell', { kind: 'abs', value: '1' }, MID, '')
    expect(bps).toEqual({ kind: 'bps', value: '7' })
  })

  it('converts a floored side from the floor it quotes at, not the bare offset', () => {
    // At 3000, 1 cNGN is 3.3 bps; the 5 bps floor is what the side quotes.
    const weak = 1 / 3000
    expect(switchUnit('sell', { kind: 'abs', value: '1' }, weak, '5')).toEqual({
      kind: 'bps',
      value: '5',
    })
    expect(switchUnit('buy', { kind: 'abs', value: '1' }, weak, '5')).toEqual({
      kind: 'bps',
      value: '5',
    })
  })

  it('clears the value with no mid to convert at', () => {
    expect(switchUnit('buy', { kind: 'bps', value: '5' }, null, '')).toEqual({ kind: 'abs', value: '' })
  })
})

describe('formatRate', () => {
  it('formats local-per-dollar rates with two decimals', () => {
    expect(formatRate(1531)).toBe('1,531.00')
    expect(formatRate(0.00065359)).toBe('0.00065359')
  })
})

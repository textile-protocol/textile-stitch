import { afterEach, describe, expect, it, vi } from 'vitest'
import { arrangeFleet, rankFleet, readFleetOrder, saveFleetOrder, type RowTotal } from './fleetOrder'
import type { Bot } from './types'

// Only the fields the ordering reads; the rest of a Bot doesn't matter here.
const bot = (name: string, running = false, displayName: string | null = null): Bot =>
  ({ name, displayName, running }) as Bot

const usd = (n: number | null, unpriced: string[] = []): RowTotal => ({ usd: n, unpriced })

describe('rankFleet', () => {
  it('puts running bots first, then funded, then empty', () => {
    const bots = [bot('empty'), bot('funded'), bot('live', true)]
    const values = { empty: usd(0), funded: usd(10), live: usd(1) }
    expect(rankFleet(bots, values)).toEqual(['live', 'funded', 'empty'])
  })

  it('orders by value within a band, richest first', () => {
    const bots = [bot('a', true), bot('b', true), bot('c', true)]
    const values = { a: usd(5), b: usd(50), c: usd(20) }
    expect(rankFleet(bots, values)).toEqual(['b', 'c', 'a'])
  })

  it('sorts an unread value to the bottom of its band', () => {
    const bots = [bot('unread', true), bot('read', true), bot('idle')]
    expect(rankFleet(bots, { read: usd(0) })).toEqual(['read', 'unread', 'idle'])
  })

  it('counts an unpriced holding as money', () => {
    const bots = [bot('empty'), bot('odd')]
    const values = { empty: usd(0), odd: usd(null, ['XYZ']) }
    expect(rankFleet(bots, values)).toEqual(['odd', 'empty'])
  })

  it('breaks ties by label', () => {
    const bots = [bot('z-id', false, 'Alpha'), bot('a-id', false, 'Bravo')]
    expect(rankFleet(bots, {})).toEqual(['z-id', 'a-id'])
  })

  it('leaves the input as it was', () => {
    const bots = [bot('b'), bot('a')]
    rankFleet(bots, {})
    expect(bots.map((b) => b.name)).toEqual(['b', 'a'])
  })
})

describe('arrangeFleet', () => {
  const names = (bots: Bot[]) => bots.map((b) => b.name)

  it('follows the saved order', () => {
    const bots = [bot('a'), bot('b'), bot('c')]
    expect(names(arrangeFleet(bots, ['c', 'a', 'b']))).toEqual(['c', 'a', 'b'])
  })

  it('skips names no longer in the fleet', () => {
    expect(names(arrangeFleet([bot('a'), bot('b')], ['gone', 'b', 'a']))).toEqual(['b', 'a'])
  })

  it('appends bots the order does not know, by label', () => {
    const bots = [bot('new2'), bot('old'), bot('new1')]
    expect(names(arrangeFleet(bots, ['old']))).toEqual(['old', 'new1', 'new2'])
  })

  it('is alphabetical with no saved order', () => {
    expect(names(arrangeFleet([bot('b'), bot('c'), bot('a')], []))).toEqual(['a', 'b', 'c'])
  })

  it('places a name listed twice once', () => {
    expect(names(arrangeFleet([bot('a'), bot('b')], ['a', 'b', 'a']))).toEqual(['a', 'b'])
  })
})

describe('readFleetOrder', () => {
  const stubStorage = (raw: string | null) => {
    const store = new Map(raw === null ? [] : [['stitch-fleet-order', raw]])
    vi.stubGlobal('window', {
      localStorage: {
        getItem: (k: string) => store.get(k) ?? null,
        setItem: (k: string, v: string) => void store.set(k, v),
      },
    })
  }

  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('round-trips a saved order', () => {
    stubStorage(null)
    saveFleetOrder(['b', 'a'])
    expect(readFleetOrder()).toEqual(['b', 'a'])
  })

  it.each([
    ['nothing saved', null],
    ['not JSON', '{oops'],
    ['not an array', '{"a":1}'],
    ['a non-string entry', '["a",2]'],
  ])('reads %s as no order', (_, raw) => {
    stubStorage(raw)
    expect(readFleetOrder()).toEqual([])
  })

  it('reads as no order when storage throws', () => {
    vi.stubGlobal('window', {
      localStorage: {
        getItem: () => {
          throw new Error('denied')
        },
      },
    })
    expect(readFleetOrder()).toEqual([])
  })
})

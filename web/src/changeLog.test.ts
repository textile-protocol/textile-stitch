import { describe, expect, it } from 'vitest'
import {
  CSV_COLUMNS,
  EMPTY_CHANGE_FILTER,
  actorLabel,
  appendOlder,
  byDay,
  changeLogCsv,
  csvCell,
  csvFilename,
  dayLabel,
  filterChangeSets,
  formatDelta,
  formatValue,
  groupChangeSets,
  paramLabel,
  sourceKind,
  summarize,
  type ChangeLogBot,
  type ChangeLogEntry,
} from './changeLog'

let n = 0
function record(patch: Partial<ChangeLogEntry> = {}): ChangeLogEntry {
  n += 1
  return {
    id: `r${n}`,
    changeSetId: 'cs1',
    changedAt: '2026-11-13T10:00:00.000Z',
    source: 'panel.settings',
    actor: 'tailnet:ana@acme.io',
    bot: 'lagos',
    chainId: 42220,
    scope: 'pool:0xc/0xd',
    scopeLabel: 'cNGN → USDT',
    parameter: 'buy_offset_bps',
    unit: 'bps',
    before: '35',
    after: '40',
    configSha256Before: 'a',
    configSha256After: 'b',
    acknowledged: false,
    ...patch,
  }
}

function logBot(patch: Partial<ChangeLogBot> = {}): ChangeLogBot {
  return {
    name: 'lagos',
    displayName: null,
    shipping: false,
    shippedThrough: null,
    refused: [],
    readError: null,
    ...patch,
  }
}

describe('groupChangeSets', () => {
  it('folds one save into one set and keeps the order it was written in', () => {
    const a = record({ parameter: 'buy_offset_bps' })
    const b = record({ parameter: 'sell_offset_bps' })
    const sets = groupChangeSets([a, b], [logBot()])
    expect(sets).toHaveLength(1)
    expect(sets[0]!.changes.map((c) => c.parameter)).toEqual(['buy_offset_bps', 'sell_offset_bps'])
  })

  it('keeps two bots apart even when their change set ids collide', () => {
    const sets = groupChangeSets(
      [record({ bot: 'lagos' }), record({ bot: 'jakarta' })],
      [logBot(), logBot({ name: 'jakarta' })],
    )
    expect(sets).toHaveLength(2)
  })

  it('sorts a host edit by when the bot picked it up, not the file time', () => {
    const panel = record({ changeSetId: 'p', changedAt: '2026-11-13T10:00:00.000Z' })
    const host = record({
      changeSetId: 'h',
      source: 'host-edit',
      changedAt: '2026-11-13T09:00:00.000Z',
      detectedAt: '2026-11-13T11:00:00.000Z',
    })
    expect(groupChangeSets([panel, host], [logBot()]).map((s) => s.id)).toEqual(['h', 'p'])
  })

  it('does not mutate the records it was given', () => {
    const records = [record({ changeSetId: 'x' }), record({ changeSetId: 'y', changedAt: '2026-11-14T00:00:00.000Z' })]
    const copy = structuredClone(records)
    groupChangeSets(records, [logBot()])
    expect(records).toEqual(copy)
  })
})

describe('ship state', () => {
  const ship = (bot: ChangeLogBot | null, ...records: ChangeLogEntry[]) =>
    groupChangeSets(records, bot ? [bot] : [])[0]!.ship

  it('is local when nothing was acknowledged and the bot does not ship, or is unknown', () => {
    expect(ship(logBot(), record())).toBe('local')
    expect(ship(null, record({ bot: 'ghost' }))).toBe('local')
  })

  it('is waiting while the bot ships and Textile has not acknowledged it', () => {
    expect(ship(logBot({ shipping: true }), record())).toBe('waiting')
  })

  it('is shipped once acknowledged, whatever the bot is set to now', () => {
    expect(ship(logBot({ shipping: true }), record({ acknowledged: true }))).toBe('shipped')
    // Shipping turned off later: Textile still has these.
    expect(ship(logBot({ shipping: false }), record({ acknowledged: true }))).toBe('shipped')
    expect(ship(null, record({ acknowledged: true }))).toBe('shipped')
  })

  it('is not shipped until every record in the set is', () => {
    const half = [record({ acknowledged: true }), record({ acknowledged: false })]
    expect(ship(logBot({ shipping: true }), ...half)).toBe('waiting')
    expect(ship(logBot({ shipping: false }), ...half)).toBe('local')
  })

  it('reports refused over everything else, even after shipping was turned off', () => {
    const r = record()
    expect(ship(logBot({ shipping: false, refused: [r.id] }), r)).toBe('refused')
    const acked = record({ acknowledged: true })
    expect(ship(logBot({ shipping: true, refused: [acked.id] }), acked)).toBe('refused')
  })
})

describe('filterChangeSets', () => {
  const sets = groupChangeSets(
    [
      record({ changeSetId: 'p', bot: 'lagos' }),
      record({ changeSetId: 'h', bot: 'lagos', source: 'host-edit', actor: 'unrecorded (host edit)', parameter: 'max_loss_bps' }),
      record({ changeSetId: 'b', bot: 'jakarta', source: 'baseline', actor: 'baseline' }),
    ],
    [logBot(), logBot({ name: 'jakarta' })],
  )
  const ids = (f: Partial<typeof EMPTY_CHANGE_FILTER>) =>
    filterChangeSets(sets, { ...EMPTY_CHANGE_FILTER, ...f }).map((s) => s.id).sort()

  it('leaves first readings out of "all" and shows them on their own', () => {
    expect(ids({})).toEqual(['h', 'p'])
    expect(ids({ kind: 'baseline' })).toEqual(['b'])
  })

  it('narrows by bot, kind and search', () => {
    expect(ids({ kind: 'host' })).toEqual(['h'])
    expect(ids({ bot: 'jakarta' })).toEqual([])
    expect(ids({ query: 'price guard' })).toEqual(['h'])
    expect(ids({ query: 'ANA@' })).toEqual(['p'])
    expect(ids({ query: 'cngn' })).toEqual(['h', 'p'])
  })
})

describe('byDay and dayLabel', () => {
  it('buckets consecutive sets by UTC day', () => {
    const sets = groupChangeSets(
      [
        record({ changeSetId: 'a', changedAt: '2026-11-13T23:59:00.000Z' }),
        record({ changeSetId: 'b', changedAt: '2026-11-13T00:01:00.000Z' }),
        record({ changeSetId: 'c', changedAt: '2026-11-12T12:00:00.000Z' }),
      ],
      [logBot()],
    )
    expect(byDay(sets).map((d) => [d.day, d.sets.length])).toEqual([
      ['2026-11-13', 2],
      ['2026-11-12', 1],
    ])
  })

  it('names today and yesterday', () => {
    const now = new Date('2026-11-13T15:00:00.000Z')
    expect(dayLabel('2026-11-13', now)).toMatch(/^Today · /)
    expect(dayLabel('2026-11-12', now)).toMatch(/^Yesterday · /)
    expect(dayLabel('2026-11-10', now)).not.toMatch(/Today|Yesterday/)
  })
})

describe('formatValue', () => {
  it('turns atomic units into token amounts on the right leg', () => {
    expect(formatValue('25000000000', 'debt token atomic units (6 decimals)', 'IDRX → USDT')).toBe('25,000 USDT')
    expect(formatValue('41000000000', 'collateral token atomic units (2 decimals)', 'IDRX → USDT')).toBe('410,000,000 IDRX')
    expect(formatValue('1500000', 'debt token atomic units (6 decimals)')).toBe('1.5')
  })

  it('formats the other units', () => {
    expect(formatValue('40', 'bps', undefined)).toBe('40 bps')
    expect(formatValue('8', 'bps per hour')).toBe('8 bps/h')
    expect(formatValue('30', 'seconds')).toBe('30s')
    expect(formatValue('true', 'bool')).toBe('on')
    expect(formatValue('false', 'bool')).toBe('off')
    expect(formatValue('0xa,0xb', 'addresses')).toBe('2 addresses')
    expect(formatValue('', 'addresses')).toBe('none')
    expect(formatValue('https://api.example.com/price?pair=x', 'url')).toBe('api.example.com/price?pair=x')
    expect(formatValue('not a url', 'url')).toBe('not a url')
    expect(formatValue(null, 'bps')).toBe('unset')
  })
})

describe('formatDelta', () => {
  it('signs bps and seconds differences', () => {
    expect(formatDelta({ before: '35', after: '45', unit: 'bps' })).toBe('+10 bps')
    expect(formatDelta({ before: '120', after: '60', unit: 'seconds' })).toBe('-60s')
  })

  it('gives a percentage for token amounts', () => {
    expect(formatDelta({ before: '25000', after: '40000', unit: 'debt token atomic units (6 decimals)' })).toBe('+60%')
    expect(formatDelta({ before: '100', after: '105', unit: 'debt token atomic units (6 decimals)' })).toBe('+5.0%')
  })

  it('has nothing to say when a side is missing, unchanged, zero or not numeric', () => {
    expect(formatDelta({ before: null, after: '40', unit: 'bps' })).toBeNull()
    expect(formatDelta({ before: '40', after: '40', unit: 'bps' })).toBeNull()
    expect(formatDelta({ before: '0', after: '5', unit: 'debt token atomic units (6 decimals)' })).toBeNull()
    expect(formatDelta({ before: 'a', after: 'b', unit: 'url' })).toBeNull()
  })
})

describe('labels', () => {
  it('names actors in words', () => {
    expect(actorLabel('tailnet:ana@acme.io')).toEqual({ who: 'ana@acme.io', how: 'tailnet' })
    expect(actorLabel('panel password (shared)')).toEqual({ who: 'Panel password', how: 'shared' })
    expect(actorLabel('unrecorded (host edit)')).toEqual({ who: 'Not recorded', how: null })
    expect(actorLabel('feed-switch-signer:0x1234567890abcdef')).toEqual({ who: '0x1234…cdef', how: 'feed switch signer' })
  })

  it('names parameters, falling back to the key in words', () => {
    expect(paramLabel('max_loss_bps')).toBe('Price guard')
    expect(paramLabel('off_hours.session_hours')).toBe('Off-hours session hours')
    expect(paramLabel('modules.curve.k')).toBe('Module curve k')
    expect(paramLabel('something_new')).toBe('Something new')
  })

  it('sorts sources into panel, host and baseline', () => {
    expect(sourceKind('baseline')).toBe('baseline')
    expect(sourceKind('host-edit')).toBe('host')
    expect(sourceKind('panel.raw')).toBe('panel')
  })
})

describe('summarize', () => {
  it('counts the week without first readings', () => {
    const sets = groupChangeSets(
      [
        record({ changeSetId: 'p', bot: 'lagos' }),
        record({ changeSetId: 'h', bot: 'jakarta', source: 'host-edit' }),
        record({ changeSetId: 'b', bot: 'accra', source: 'baseline' }),
        record({ changeSetId: 'old', bot: 'lagos', changedAt: '2026-10-01T00:00:00.000Z' }),
      ],
      [],
    )
    expect(summarize(sets, '2026-11-06T00:00:00.000Z', false)).toEqual({
      sets: 2,
      hostEdits: 1,
      bots: 2,
      complete: true,
    })
  })

  it('is a floor while older pages could still hold changes from the window', () => {
    const recent = groupChangeSets([record({ changeSetId: 'p' })], [])
    const since = '2026-11-06T00:00:00.000Z'
    expect(summarize(recent, since, true).complete).toBe(false)
    expect(summarize(recent, since, false).complete).toBe(true)
    const reachesBack = groupChangeSets(
      [record({ changeSetId: 'p' }), record({ changeSetId: 'old', changedAt: '2026-10-01T00:00:00.000Z' })],
      [],
    )
    expect(summarize(reachesBack, since, true).complete).toBe(true)
  })
})

describe('appendOlder', () => {
  it('adds the older records under the newer ones, once each, with the older cursor', () => {
    const a = record({ id: 'a' })
    const b = record({ id: 'b' })
    const c = record({ id: 'c' })
    const merged = appendOlder(
      { records: [a, b], bots: [logBot({ shipping: true })], nextCursor: 'p2' },
      { records: [b, c], bots: [logBot()], nextCursor: null },
    )
    expect(merged.records.map((r) => r.id)).toEqual(['a', 'b', 'c'])
    expect(merged.bots[0]!.shipping).toBe(true)
    expect(merged.nextCursor).toBeNull()
  })
})

describe('csvCell', () => {
  it('quotes commas, quotes and newlines', () => {
    expect(csvCell('a,b')).toBe('"a,b"')
    expect(csvCell('say "hi"')).toBe('"say ""hi"""')
    expect(csvCell('two\nlines')).toBe('"two\nlines"')
  })

  it('defuses formulas but leaves plain numbers alone', () => {
    expect(csvCell('=HYPERLINK("x")')).toBe(`"'=HYPERLINK(""x"")"`)
    expect(csvCell('@SUM(A1)')).toBe("'@SUM(A1)")
    expect(csvCell('+1-2')).toBe("'+1-2")
    expect(csvCell('-1000')).toBe('-1000')
    expect(csvCell('-0.5')).toBe('-0.5')
  })

  it('writes unset as an empty cell', () => {
    expect(csvCell(null)).toBe('')
    expect(csvCell('')).toBe('')
  })
})

describe('changeLogCsv', () => {
  it('writes one row per parameter, oldest set first, raw values', () => {
    const bots = [logBot({ shipping: false })]
    const sets = groupChangeSets(
      [
        record({ id: 'new1', changeSetId: 'new', changedAt: '2026-11-13T12:00:00.000Z', parameter: 'buy_offset_bps', acknowledged: true }),
        record({ id: 'new2', changeSetId: 'new', changedAt: '2026-11-13T12:00:00.000Z', parameter: 'sell_offset_bps' }),
        record({
          id: 'old1',
          changeSetId: 'old',
          changedAt: '2026-11-12T09:00:00.000Z',
          unit: 'debt token atomic units (6 decimals)',
          parameter: 'buy_total_liquidity_debt',
          before: null,
          after: '25000000000',
          acknowledged: true,
        }),
      ],
      bots,
    )
    const lines = changeLogCsv(sets, bots).split('\r\n')
    expect(lines[0]).toBe(CSV_COLUMNS.join(','))
    expect(lines.slice(1, -1).map((l) => l.split(',').at(-1))).toEqual(['old1', 'new1', 'new2'])
    const cell = (line: string, col: (typeof CSV_COLUMNS)[number]) =>
      line.split(',')[CSV_COLUMNS.indexOf(col)]
    expect(cell(lines[1]!, 'bot')).toBe('lagos')
    expect(cell(lines[1]!, 'value_before')).toBe('')
    expect(cell(lines[1]!, 'value_after')).toBe('25000000000')
    expect(cell(lines[1]!, 'sent_to_textile')).toBe('yes')
    // Per record: the set the cursor cut in half says which row Textile has,
    // and acknowledged rows stay "yes" with shipping now off.
    expect(cell(lines[2]!, 'sent_to_textile')).toBe('yes')
    expect(cell(lines[3]!, 'sent_to_textile')).toBe('no')
    expect(lines.at(-1)).toBe('')
  })

  it('is just the header with nothing to export', () => {
    expect(changeLogCsv([], [])).toBe(`${CSV_COLUMNS.join(',')}\r\n`)
  })
})

describe('csvFilename', () => {
  it('names the bot or the fleet and the UTC day', () => {
    const now = new Date('2026-11-16T23:30:00.000Z')
    expect(csvFilename('lagos-cngn', now)).toBe('stitch-change-log_lagos-cngn_2026-11-16.csv')
    expect(csvFilename('', now)).toBe('stitch-change-log_fleet_2026-11-16.csv')
    expect(csvFilename('a/b c', now)).toBe('stitch-change-log_a-b-c_2026-11-16.csv')
  })
})

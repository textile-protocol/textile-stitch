// The parameter change log, as the panel shows it. Pure: grouping, labels and
// value formatting over the records the bots keep in `change-log.jsonl`.
// Nothing here fetches.

/** One line of a bot's `change-log.jsonl`, as written (see changelog/record.rs). */
export interface ChangeRecord {
  id: string
  changeSetId: string
  /** ISO-8601 UTC. For a host edit, the file's mtime. */
  changedAt: string
  /** When a host edit was noticed: the bot starting or the next panel save. */
  detectedAt?: string
  source: string
  actor: string
  bot?: string
  chainId: number
  scope: string
  scopeLabel?: string
  parameter: string
  unit: string
  before: string | null
  after: string | null
  configSha256Before: string | null
  configSha256After: string | null
}

/**
 * A journal line as the panel serves it: the record, plus whether Textile has
 * acknowledged it. The panel reads that from the bot's `change-log.shipped`
 * cursor, so it stays true after shipping is turned off: those records did
 * reach Textile.
 */
export interface ChangeLogEntry extends ChangeRecord {
  acknowledged: boolean
}

/** Whether a bot sends its records to Textile, and how far Textile has them. */
export interface ChangeLogBot {
  name: string
  displayName: string | null
  /** `[change_log] ship` as configured now. Says nothing about past records. */
  shipping: boolean
  /**
   * When the last record Textile acknowledged took effect (`detectedAt`, else
   * `changedAt`). For display only; per-record state is `acknowledged`.
   * Null when Textile has nothing from this bot.
   */
  shippedThrough: string | null
  /** Ids listed in `change-log.refused`. */
  refused: string[]
  /** Why the panel couldn't read this bot's log, when it couldn't. */
  readError: string | null
}

/** GET /api/changes */
export interface ChangeLogPage {
  records: ChangeLogEntry[]
  bots: ChangeLogBot[]
  /** Pass back as `before` for older records. Null at the start of every log. */
  nextCursor: string | null
}

export type ShipState = 'local' | 'shipped' | 'waiting' | 'refused'

/** Everything one save changed. */
export interface ChangeSet {
  id: string
  bot: string
  changedAt: string
  detectedAt: string | null
  source: string
  actor: string
  changes: ChangeLogEntry[]
  ship: ShipState
}

export type SourceKind = 'panel' | 'host' | 'baseline'

export interface ChangeFilter {
  bot: string
  kind: 'all' | SourceKind
  query: string
}

export const EMPTY_CHANGE_FILTER: ChangeFilter = {
  bot: '',
  kind: 'all',
  query: '',
}

/** Records grouped by change set, newest first. Order inside a set is as written. */
export function groupChangeSets(
  records: readonly ChangeLogEntry[],
  bots: readonly ChangeLogBot[],
): ChangeSet[] {
  const byBot = new Map(bots.map((b) => [b.name, b]))
  const sets = records.reduce((acc, r) => {
    const key = `${r.bot ?? ''}:${r.changeSetId}`
    const prev = acc.get(key)
    return new Map(acc).set(key, prev ? [...prev, r] : [r])
  }, new Map<string, ChangeLogEntry[]>())
  return [...sets.values()]
    .map((changes) => {
      const first = changes[0]!
      const bot = first.bot ?? ''
      return {
        id: first.changeSetId,
        bot,
        changedAt: first.changedAt,
        detectedAt: first.detectedAt ?? null,
        source: first.source,
        actor: first.actor,
        changes,
        ship: shipState(changes, byBot.get(bot)),
      }
    })
    .sort((a, b) => effectiveAt(b).localeCompare(effectiveAt(a)))
}

/** A host edit sorts by when the bot picked it up, which is when it took effect. */
function effectiveAt(set: Pick<ChangeSet, 'changedAt' | 'detectedAt'>): string {
  return set.detectedAt ?? set.changedAt
}

/**
 * Where a set stands with Textile. What Textile acknowledged or refused is
 * history and wins over the current setting; only what it hasn't seen depends
 * on whether the bot ships now. A set the cursor cut in half isn't all there
 * yet, so it reads as waiting (or local, when shipping is off).
 */
function shipState(changes: readonly ChangeLogEntry[], bot: ChangeLogBot | undefined): ShipState {
  if (bot && changes.some((c) => bot.refused.includes(c.id))) return 'refused'
  if (changes.every((c) => c.acknowledged)) return 'shipped'
  return bot?.shipping ? 'waiting' : 'local'
}

/** The same rule for one record, for the CSV. */
function recordShipState(c: ChangeLogEntry, bot: ChangeLogBot | undefined): ShipState {
  return shipState([c], bot)
}

export function sourceKind(source: string): SourceKind {
  if (source === 'baseline') return 'baseline'
  if (source === 'host-edit') return 'host'
  return 'panel'
}

export function filterChangeSets(sets: readonly ChangeSet[], f: ChangeFilter): ChangeSet[] {
  const q = f.query.trim().toLowerCase()
  return sets
    .filter((s) => !f.bot || s.bot === f.bot)
    // A first reading lists every value the bot started with. It's history,
    // not a change anyone made, so "all" leaves it out.
    .filter((s) =>
      f.kind === 'all' ? sourceKind(s.source) !== 'baseline' : sourceKind(s.source) === f.kind,
    )
    .filter(
      (s) =>
        !q ||
        s.actor.toLowerCase().includes(q) ||
        s.changes.some(
          (c) =>
            c.parameter.toLowerCase().includes(q) ||
            paramLabel(c.parameter).toLowerCase().includes(q) ||
            (c.scopeLabel ?? '').toLowerCase().includes(q),
        ),
    )
}

/** Day buckets in UTC, the clock the records and the monthly export use. */
export function byDay(sets: readonly ChangeSet[]): { day: string; sets: ChangeSet[] }[] {
  return sets.reduce<{ day: string; sets: ChangeSet[] }[]>((days, s) => {
    const day = effectiveAt(s).slice(0, 10)
    const last = days[days.length - 1]
    return last && last.day === day
      ? [...days.slice(0, -1), { day, sets: [...last.sets, s] }]
      : [...days, { day, sets: [s] }]
  }, [])
}

export function dayLabel(day: string, now: Date): string {
  const today = now.toISOString().slice(0, 10)
  const yesterday = new Date(now.getTime() - 86_400_000).toISOString().slice(0, 10)
  const date = new Date(`${day}T00:00:00Z`).toLocaleDateString('en-GB', {
    weekday: 'short',
    day: 'numeric',
    month: 'short',
    timeZone: 'UTC',
  })
  if (day === today) return `Today · ${date}`
  if (day === yesterday) return `Yesterday · ${date}`
  return date
}

export function timeUtc(iso: string): string {
  return iso.slice(11, 16)
}

const SOURCE_LABEL: Record<string, string> = {
  baseline: 'First reading',
  'host-edit': 'Edited on the host',
  'panel.settings': 'Settings',
  'panel.raw': 'Raw config',
  'panel.pools.add': 'Corridor added',
  'panel.pools.remove': 'Corridor removed',
  'panel.modules': 'Modules',
  'panel.modules.enabled': 'Modules switch',
  'panel.enroll': 'Connect',
  'panel.verify': 'Email confirm',
  'panel.vault': 'Vault',
  'panel.perps': 'FX hedge',
  'panel.identity': 'Agent identity',
  'panel.rollback': 'Rolled back',
  'panel.create': 'New bot',
  'api.feed-switch': 'Feed switch',
  'bot.issuer-spread': 'Issuer spread',
}

export function sourceLabel(source: string): string {
  return SOURCE_LABEL[source] ?? source
}

/** Who did it, in words. The raw actor stays on hover. */
export function actorLabel(actor: string): { who: string; how: string | null } {
  if (actor.startsWith('tailnet:')) return { who: actor.slice(8), how: 'tailnet' }
  if (actor === 'panel password (shared)') return { who: 'Panel password', how: 'shared' }
  if (actor === 'unrecorded (host edit)') return { who: 'Not recorded', how: null }
  if (actor === 'baseline') return { who: 'Stitch', how: null }
  if (actor.startsWith('feed-switch-signer:'))
    return { who: shortHex(actor.slice(19)), how: 'feed switch signer' }
  return { who: actor, how: null }
}

const PARAM_LABEL: Record<string, string> = {
  buy_offset_bps: 'Buy spread',
  sell_offset_bps: 'Sell spread',
  buy_offset_abs: 'Buy offset',
  sell_offset_abs: 'Sell offset',
  buy_total_liquidity_debt: 'Buy liquidity',
  sell_total_liquidity_collateral: 'Sell liquidity',
  buy_order_size_debt: 'Buy order size',
  sell_order_size_collateral: 'Sell order size',
  feed_url: 'Price feed',
  feed_sides: 'Quote feed bid/ask',
  rfq_staleness_secs: 'RFQ freshness',
  issuer_spread_min_bps: 'Issuer spread min',
  issuer_spread_max_bps: 'Issuer spread max',
  max_loss_bps: 'Price guard',
  max_jump_bps: 'Jump guard',
  settle_secs: 'Settle window',
  staleness_secs: 'Feed staleness',
  rfq_enabled: 'Swap quoting',
  'rfq.allowed_takers': 'Allowed takers',
  book_enabled: 'Limit-order taking',
  modules_enabled: 'Modules',
  'change_log.ship': 'Ship change log',
  'feed_switch.selected': 'Feed switch venue',
  'off_hours.widen_bps_per_hour': 'Off-hours widening',
  'off_hours.max_bps': 'Off-hours max widening',
  'issuer_spread.revision': 'Issuer spread revision',
}

export function paramLabel(parameter: string): string {
  if (PARAM_LABEL[parameter]) return PARAM_LABEL[parameter]
  if (parameter.startsWith('off_hours.')) return `Off-hours ${words(parameter.slice(10))}`
  if (parameter.startsWith('modules.')) return `Module ${words(parameter.slice(8))}`
  const s = words(parameter)
  return s.charAt(0).toUpperCase() + s.slice(1)
}

function words(key: string): string {
  return key.replace(/[._]/g, ' ')
}

/** "cNGN → USDT" and "collateral" → "cNGN". */
function legSymbol(scopeLabel: string | undefined, leg: string): string {
  const [collateral, debt] = (scopeLabel ?? '').split(/\s*(?:→|\/)\s*/)
  return (leg === 'collateral' ? collateral : debt) ?? ''
}

/** A value as an operator reads it: bps, token amounts, on/off. */
export function formatValue(value: string | null, unit: string, scopeLabel?: string): string {
  if (value === null) return 'unset'
  const atomic = unit.match(/^(debt|collateral) token atomic units \((\d+) decimals\)$/)
  if (atomic) {
    const amount = fromAtomic(value, Number(atomic[2]))
    const symbol = legSymbol(scopeLabel, atomic[1]!)
    return symbol ? `${amount} ${symbol}` : amount
  }
  if (unit === 'bps') return `${value} bps`
  if (unit === 'bps per hour') return `${value} bps/h`
  if (unit === 'seconds') return `${value}s`
  if (unit === 'hours') return `${value}h`
  if (unit === 'bool') return value === 'true' ? 'on' : 'off'
  if (unit === 'addresses') {
    const n = value ? value.split(',').length : 0
    return n === 0 ? 'none' : `${n} address${n === 1 ? '' : 'es'}`
  }
  if (unit === 'url') return hostAndPath(value)
  return value
}

/** Signed difference for numeric units, e.g. "+5 bps". Null when it doesn't apply. */
export function formatDelta(c: Pick<ChangeRecord, 'before' | 'after' | 'unit' | 'scopeLabel'>): string | null {
  if (c.before === null || c.after === null) return null
  const atomic = c.unit.match(/^(debt|collateral) token atomic units \((\d+) decimals\)$/)
  if (atomic) {
    const before = Number(c.before)
    const after = Number(c.after)
    if (!Number.isFinite(before) || !Number.isFinite(after) || before === 0) return null
    const pct = ((after - before) / before) * 100
    return `${pct > 0 ? '+' : ''}${pct.toFixed(Math.abs(pct) < 10 ? 1 : 0)}%`
  }
  if (c.unit !== 'bps' && c.unit !== 'seconds') return null
  const d = Number(c.after) - Number(c.before)
  if (!Number.isFinite(d) || d === 0) return null
  return `${d > 0 ? '+' : ''}${d}${c.unit === 'bps' ? ' bps' : 's'}`
}

function fromAtomic(value: string, decimals: number): string {
  const n = Number(value) / 10 ** decimals
  if (!Number.isFinite(n)) return value
  return n.toLocaleString('en-US', { maximumFractionDigits: n >= 1000 ? 0 : 2 })
}

function hostAndPath(url: string): string {
  try {
    const u = new URL(url)
    return `${u.host}${u.pathname}${u.search}`
  } catch {
    return url
  }
}

function shortHex(a: string): string {
  return a.length > 12 ? `${a.slice(0, 6)}…${a.slice(-4)}` : a
}

/** Counts for the summary line over the sets currently loaded. */
export function summarize(sets: readonly ChangeSet[], since: string) {
  const recent = sets.filter((s) => effectiveAt(s) >= since && sourceKind(s.source) !== 'baseline')
  return {
    sets: recent.length,
    hostEdits: recent.filter((s) => sourceKind(s.source) === 'host').length,
    bots: new Set(recent.map((s) => s.bot)).size,
  }
}

/**
 * Add an older page under the one already loaded. Records are de-duplicated
 * by id, in case the log grew between the two reads. The bots' ship state
 * comes from the newer page, which is the fresher read.
 */
export function appendOlder(newer: ChangeLogPage, older: ChangeLogPage): ChangeLogPage {
  const seen = new Set(newer.records.map((r) => r.id))
  return {
    records: [...newer.records, ...older.records.filter((r) => !seen.has(r.id))],
    bots: newer.bots,
    nextCursor: older.nextCursor,
  }
}

/**
 * CSV columns. Where a column also exists in Textile's monthly export
 * (`api/src/lib/stitchChangeLog/exportFormat.ts`) it has the same name and
 * holds the same raw value, so the two files line up. `sent_to_textile` is
 * the panel's own: what this host knows about shipping.
 */
export const CSV_COLUMNS = [
  'changed_at_utc',
  'detected_at_utc',
  'bot',
  'chain_id',
  'scope',
  'scope_label',
  'parameter',
  'unit',
  'value_before',
  'value_after',
  'source',
  'actor',
  'sent_to_textile',
  'change_set_id',
  'record_id',
] as const

const SHIP_CSV: Record<ShipState, string> = {
  local: 'no',
  shipped: 'yes',
  waiting: 'pending',
  refused: 'refused',
}

const DECIMAL = /^-?\d+(\.\d+)?$/

/**
 * One CSV cell: RFC 4180 quoting, and a leading apostrophe on anything a
 * spreadsheet would run as a formula that isn't a plain number. Actors and
 * feed URLs come from operators, and this file gets opened in Excel. Same
 * rule as Textile's export. An empty cell means unset.
 */
export function csvCell(value: string | null): string {
  if (value === null) return ''
  const guarded = /^[=+\-@\t\r]/.test(value) && !DECIMAL.test(value.trim()) ? `'${value}` : value
  return /[",\r\n]/.test(guarded) ? `"${guarded.replace(/"/g, '""')}"` : guarded
}

/**
 * The sets as CSV, one row per parameter, oldest first, raw values.
 * `sent_to_textile` is per record, so a set the cursor cut in half says which
 * rows Textile has.
 */
export function changeLogCsv(sets: readonly ChangeSet[], bots: readonly ChangeLogBot[]): string {
  const byBot = new Map(bots.map((b) => [b.name, b]))
  const rows = [...sets].reverse().flatMap((s) =>
    s.changes.map((c) => {
      const row: Record<(typeof CSV_COLUMNS)[number], string | null> = {
        changed_at_utc: c.changedAt,
        detected_at_utc: c.detectedAt ?? null,
        bot: s.bot,
        chain_id: String(c.chainId),
        scope: c.scope,
        scope_label: c.scopeLabel ?? null,
        parameter: c.parameter,
        unit: c.unit,
        value_before: c.before,
        value_after: c.after,
        source: c.source,
        actor: c.actor,
        sent_to_textile: SHIP_CSV[recordShipState(c, byBot.get(s.bot))],
        change_set_id: c.changeSetId,
        record_id: c.id,
      }
      return CSV_COLUMNS.map((col) => csvCell(row[col])).join(',')
    }),
  )
  return [CSV_COLUMNS.join(','), ...rows].join('\r\n') + '\r\n'
}

/** `stitch-change-log_lagos-cngn_2026-11-16.csv`, or `_fleet_` for every bot. */
export function csvFilename(bot: string, now: Date): string {
  const who = bot.replace(/[^A-Za-z0-9._-]/g, '-') || 'fleet'
  return `stitch-change-log_${who}_${now.toISOString().slice(0, 10)}.csv`
}

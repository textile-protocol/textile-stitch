import { useCallback, useEffect, useMemo, useState } from 'react'
import { Link } from 'react-router-dom'
import { ApiError, api } from '../api'
import { botPath } from '../botRoutes'
import {
  EMPTY_CHANGE_FILTER,
  actorLabel,
  appendOlder,
  byDay,
  changeLogCsv,
  csvFilename,
  dayLabel,
  filterChangeSets,
  formatDelta,
  formatValue,
  groupChangeSets,
  paramLabel,
  sourceKind,
  sourceLabel,
  summarize,
  timeUtc,
  type ChangeFilter,
  type ChangeLogBot,
  type ChangeLogPage,
  type ChangeSet,
  type ShipState,
} from '../changeLog'
import { Banner, Button, Card, Empty, ErrorState, Loading } from './ui'

/** Rows a change set shows before "Show n more". */
const COLLAPSED_ROWS = 4

/**
 * The parameter change log: every change to a spread, size cap, the price
 * guard or a feed, read from each bot's `change-log.jsonl`. With `bot` set it
 * is that bot's log, without it the whole fleet's, newest first.
 *
 * Read-only. The panel reads the journals it already owns; nothing here goes
 * to Textile.
 */
export default function ChangeLog({ bot }: { bot?: string }) {
  const [page, setPage] = useState<ChangeLogPage | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [filter, setFilter] = useState<ChangeFilter>({ ...EMPTY_CHANGE_FILTER, bot: bot ?? '' })
  const [loadingOlder, setLoadingOlder] = useState(false)

  const load = useCallback(async () => {
    try {
      setPage(await api.changes({ bot }))
      setError(null)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    }
  }, [bot])

  useEffect(() => {
    void load()
  }, [load])

  // An error here keeps what's loaded on screen and says why the next page
  // didn't come; the button stays so the operator can try again.
  const loadOlder = async (current: ChangeLogPage) => {
    if (!current.nextCursor) return
    setLoadingOlder(true)
    try {
      const older = await api.changes({ bot, before: current.nextCursor })
      setPage((p) => (p === current ? appendOlder(current, older) : p))
      setError(null)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setLoadingOlder(false)
    }
  }

  const sets = useMemo(
    () => (page ? groupChangeSets(page.records, page.bots) : []),
    [page],
  )

  if (!page && error) return <ErrorState error={error} onRetry={() => void load()} />
  if (!page) return <Loading what="the change log" />

  const shown = filterChangeSets(sets, filter)
  const labels = new Map(page.bots.map((b) => [b.name, b.displayName?.trim() || b.name]))
  const refused = page.bots.filter((b) => b.refused.length > 0)
  const unreadable = page.bots.filter((b) => b.readError)
  const weekAgo = new Date(Date.now() - 7 * 86_400_000).toISOString()
  const week = summarize(sets, weekAgo, page.nextCursor !== null)
  const single = bot ? page.bots.find((b) => b.name === bot) : undefined

  return (
    <div className="space-y-4">
      {single ? (
        <ShippingCard bot={single} />
      ) : (
        <FleetSummary bots={page.bots} week={week} />
      )}

      {error && (
        <Banner tone="danger" onDismiss={() => setError(null)}>
          {error}
        </Banner>
      )}
      {refused.map((b) => (
        <Banner key={b.name} tone="danger">
          Textile refused {b.refused.length === 1 ? 'one record' : `${b.refused.length} records`} from{' '}
          <span className="font-bold">{labels.get(b.name)}</span>. They stay in the bot's own log,
          marked below, and the panel won't delete this bot's config while they're listed.
        </Banner>
      ))}
      {unreadable.map((b) => (
        <Banner key={b.name} tone="warning">
          Couldn't read <span className="font-bold">{labels.get(b.name)}</span>'s log: {b.readError}
        </Banner>
      ))}

      <Filters
        filter={filter}
        onChange={setFilter}
        bots={bot ? [] : page.bots.map((b) => ({ value: b.name, label: labels.get(b.name)! }))}
        counts={kindCounts(sets, filter.bot)}
        onExport={() =>
          downloadText(csvFilename(bot ?? filter.bot, new Date()), changeLogCsv(shown, page.bots))
        }
        canExport={shown.length > 0}
      />

      {shown.length === 0 ? (
        <Empty title="No changes match">
          <p>
            {filter.kind === 'all' && !filter.query
              ? 'Nothing has changed since the first reading.'
              : 'Try another filter, or look at the first readings.'}
          </p>
        </Empty>
      ) : (
        <div className="space-y-6">
          {byDay(shown).map(({ day, sets: daySets }) => (
            <section key={day}>
              <h3 className="mb-2 text-xs font-bold tracking-wide text-faint uppercase">
                {dayLabel(day, new Date())}
              </h3>
              <ol className="relative space-y-3 sm:before:absolute before:top-2 before:bottom-2 before:left-[64.5px] before:w-px before:bg-line-soft">
                {daySets.map((set) => (
                  <li key={`${set.bot}:${set.id}`}>
                    <ChangeSetRow
                      set={set}
                      botName={labels.get(set.bot) ?? set.bot}
                      showBot={!bot}
                    />
                  </li>
                ))}
              </ol>
            </section>
          ))}
          {page.nextCursor && (
            <div className="flex justify-center">
              <Button busy={loadingOlder} onClick={() => void loadOlder(page)}>
                Older changes
              </Button>
            </div>
          )}
        </div>
      )}
      <p className="text-xs text-faint">
        Times in UTC, the clock the log and Textile's monthly export use. Records stay in each
        bot's <code className="font-mono">change-log.jsonl</code>, beside its{' '}
        <code className="font-mono">stitch.toml</code>.
      </p>
    </div>
  )
}

function FleetSummary({
  bots,
  week,
}: {
  bots: readonly ChangeLogBot[]
  week: ReturnType<typeof summarize>
}) {
  const shipping = bots.filter((b) => b.shipping).length
  const everShipped = bots.some((b) => b.shippedThrough)
  return (
    <div className="flex flex-wrap items-center gap-x-6 gap-y-2 rounded-xl border border-line-soft bg-surface px-4 py-3 text-sm">
      <Stat
        value={week.sets}
        floor={!week.complete}
        label={`${week.sets === 1 ? 'change' : 'changes'} in 7 days`}
      />
      <Stat value={week.bots} floor={!week.complete} label={`of ${bots.length} bots touched`} />
      <Stat
        value={week.hostEdits}
        floor={!week.complete}
        label={week.hostEdits === 1 ? 'edit on the host' : 'edits on the host'}
        tone={week.hostEdits > 0 ? 'text-warning' : undefined}
      />
      <span className="text-muted sm:ml-auto">
        {shipping === 0
          ? everShipped
            ? 'No bot sends new records to Textile.'
            : 'Every log stays on this machine.'
          : `${shipping} of ${bots.length} bots send their log to Textile.`}
      </span>
    </div>
  )
}

/** `floor`: older pages aren't loaded yet, so the count can only go up. */
function Stat({
  value,
  label,
  tone,
  floor = false,
}: {
  value: number
  label: string
  tone?: string
  floor?: boolean
}) {
  return (
    <span
      className="flex items-baseline gap-1.5"
      title={floor ? 'At least this many. Load older changes for the full count.' : undefined}
    >
      <span className={`text-base font-bold tabular-nums ${tone ?? ''}`}>
        {value}
        {floor && '+'}
      </span>
      <span className="text-muted">{label}</span>
    </span>
  )
}

/** Through when Textile has this bot's records, in words. */
function acknowledgedUpTo(at: string) {
  return `Textile has everything up to ${timeUtc(at)} UTC on ${at.slice(0, 10)}.`
}

/**
 * Three cases, not two: a bot that stopped shipping still has records
 * Textile received, and the card mustn't say they never left the host.
 */
function ShippingCard({ bot }: { bot: ChangeLogBot }) {
  const stopped = !bot.shipping && bot.shippedThrough !== null
  return (
    <Card className="!p-4">
      <div className="flex flex-wrap items-start gap-3">
        <div className="min-w-0 flex-1 space-y-1">
          <p className="text-sm font-bold">
            {bot.shipping ? 'Sent to Textile' : stopped ? 'No longer sent to Textile' : 'Kept on this machine'}
          </p>
          <p className="text-sm text-muted">
            {bot.shipping ? (
              <>
                {bot.shippedThrough ? acknowledgedUpTo(bot.shippedThrough) : 'Nothing acknowledged yet.'}{' '}
                The bot sends new records every minute.
              </>
            ) : stopped ? (
              <>
                {acknowledgedUpTo(bot.shippedThrough!)} Changes after shipping was turned off stay on
                this machine.
              </>
            ) : (
              <>
                Nothing leaves the host. Set <code className="font-mono">[change_log] ship = true</code>{' '}
                if your agreement with Textile needs it.
              </>
            )}
          </p>
        </div>
        <ShipPill state={bot.shipping ? 'shipped' : 'local'} />
      </div>
    </Card>
  )
}

const KINDS: { value: ChangeFilter['kind']; label: string }[] = [
  { value: 'all', label: 'All changes' },
  { value: 'panel', label: 'Panel' },
  { value: 'host', label: 'On the host' },
  { value: 'baseline', label: 'First readings' },
]

function kindCounts(sets: readonly ChangeSet[], bot: string): Record<ChangeFilter['kind'], number> {
  const mine = sets.filter((s) => !bot || s.bot === bot)
  const count = (k: string) => mine.filter((s) => sourceKind(s.source) === k).length
  return {
    all: mine.length - count('baseline'),
    panel: count('panel'),
    host: count('host'),
    baseline: count('baseline'),
  }
}

function Filters({
  filter,
  onChange,
  bots,
  counts,
  onExport,
  canExport,
}: {
  filter: ChangeFilter
  onChange: (f: ChangeFilter) => void
  bots: { value: string; label: string }[]
  counts: Record<ChangeFilter['kind'], number>
  onExport: () => void
  canExport: boolean
}) {
  const set = (patch: Partial<ChangeFilter>) => onChange({ ...filter, ...patch })
  return (
    <div className="flex flex-wrap items-center gap-2">
      <label className="relative flex h-9 min-w-48 basis-full items-center sm:max-w-xs sm:basis-auto sm:flex-1">
        <svg aria-hidden viewBox="0 0 16 16" className="pointer-events-none absolute left-3 size-3.5 text-faint" fill="none" stroke="currentColor" strokeWidth="1.8">
          <circle cx="7" cy="7" r="4.5" />
          <path d="m10.5 10.5 3 3" strokeLinecap="round" />
        </svg>
        <input
          value={filter.query}
          onChange={(e) => set({ query: e.target.value })}
          placeholder="Parameter, pair or person"
          aria-label="Search changes"
          className="h-9 w-full rounded-lg border border-line bg-surface pr-3 pl-8 text-sm placeholder:text-faint"
        />
      </label>
      {bots.length > 0 && (
        <select
          value={filter.bot}
          onChange={(e) => set({ bot: e.target.value })}
          aria-label="Bot"
          className="h-9 rounded-lg border border-line bg-surface px-3 text-sm"
        >
          <option value="">All bots</option>
          {bots.map((b) => (
            <option key={b.value} value={b.value}>
              {b.label}
            </option>
          ))}
        </select>
      )}
      <div
        role="radiogroup"
        aria-label="Made through"
        className="inline-flex h-9 max-w-full items-center overflow-x-auto rounded-lg border border-line bg-surface p-0.5 text-sm whitespace-nowrap [scrollbar-width:none]"
      >
        {KINDS.map((k) => (
          <button
            key={k.value}
            type="button"
            role="radio"
            aria-checked={filter.kind === k.value}
            onClick={() => set({ kind: k.value })}
            className={`inline-flex h-full shrink-0 items-center gap-1.5 rounded-md px-2.5 font-bold ${
              filter.kind === k.value ? 'bg-accent-tint text-accent' : 'text-muted hover:text-ink'
            }`}
          >
            {k.value === 'host' && <span className="size-1.5 rounded-full bg-warning" />}
            {k.label}
            <span className="text-xs font-normal tabular-nums opacity-70">{counts[k.value]}</span>
          </button>
        ))}
      </div>
      <Button
        className="h-9 sm:ml-auto"
        onClick={onExport}
        disabled={!canExport}
        title="Download the changes shown, as CSV, one row per parameter"
      >
        Export CSV
      </Button>
    </div>
  )
}

const DOT: Record<string, string> = {
  panel: 'bg-accent',
  host: 'bg-warning',
  baseline: 'bg-faint',
}

function ChangeSetRow({
  set,
  botName,
  showBot,
}: {
  set: ChangeSet
  botName: string
  showBot: boolean
}) {
  const kind = sourceKind(set.source)
  const [open, setOpen] = useState(kind !== 'baseline')
  const [all, setAll] = useState(false)
  const actor = actorLabel(set.actor)
  const rows = all ? set.changes : set.changes.slice(0, COLLAPSED_ROWS)
  const hidden = set.changes.length - rows.length

  return (
    <div className="flex gap-3">
      <div className="hidden w-12 shrink-0 flex-col items-end pt-3 sm:flex">
        <time
          dateTime={set.detectedAt ?? set.changedAt}
          title={set.detectedAt ?? set.changedAt}
          className="font-mono text-xs text-muted tabular-nums"
        >
          {timeUtc(set.detectedAt ?? set.changedAt)}
        </time>
      </div>
      <span
        aria-hidden
        className={`relative z-10 mt-4 hidden size-2.5 shrink-0 rounded-full ring-4 ring-canvas sm:block ${DOT[kind]}`}
      />
      <div
        className={`min-w-0 flex-1 overflow-hidden rounded-xl border bg-surface ${
          set.ship === 'refused'
            ? 'border-danger/40'
            : kind === 'host'
              ? 'border-warning/40'
              : 'border-line-soft'
        }`}
      >
        <header className="flex flex-wrap items-center gap-x-2 gap-y-1 px-4 py-2.5 text-sm">
          <span className={`flex items-center gap-1.5 font-mono text-xs text-muted sm:hidden`}>
            <span aria-hidden className={`size-2 rounded-full ${DOT[kind]}`} />
            {timeUtc(set.detectedAt ?? set.changedAt)}
          </span>
          {showBot && (
            <Link to={botPath(set.bot, 'changes')} className="font-bold text-ink no-underline hover:underline">
              {botName}
            </Link>
          )}
          <SourcePill source={set.source} />
          <span className="text-muted" title={set.actor}>
            {kind === 'baseline' ? 'every value as found when the log started' : kind === 'host' ? 'no one recorded' : (
              <>
                by <span className="text-ink">{actor.who}</span>
                {actor.how && <span className="text-faint"> · {actor.how}</span>}
              </>
            )}
          </span>
          <span className="ml-auto flex items-center gap-2">
            <span className="text-xs text-faint tabular-nums">
              {set.changes.length} {set.changes.length === 1 ? 'value' : 'values'}
            </span>
            <ShipPill state={set.ship} />
          </span>
        </header>

        {kind === 'host' && (
          <p className="border-t border-line-soft bg-warning-bg/50 px-4 py-2 text-xs text-warning">
            The file changed at {timeUtc(set.changedAt)} with nothing recording who did it (an
            editor on the host, <code className="font-mono">stitch connect</code> or a restored
            backup).{' '}
            {set.detectedAt && <>The bot picked it up at {timeUtc(set.detectedAt)} when it started.</>}
          </p>
        )}

        {open ? (
          <div className="border-t border-line-soft">
            <ul className="text-sm">
              {rows.map((c) => {
                const added = c.before === null && kind !== 'baseline'
                const delta = added ? 'new' : formatDelta(c)
                return (
                  <li
                    key={c.id}
                    className="grid grid-cols-[1fr_auto] items-baseline gap-x-3 border-t border-line-soft px-4 py-2 first:border-t-0 sm:grid-cols-[8.5rem_1fr_auto_4.5rem]"
                  >
                    <span className="col-span-2 text-xs text-muted sm:col-span-1">
                      {c.scope === 'bot' ? 'Bot-wide' : (c.scopeLabel ?? c.scope)}
                    </span>
                    <span className="min-w-0">
                      <span className="font-bold">{paramLabel(c.parameter)}</span>
                      <span className="ml-2 hidden font-mono text-xs text-faint sm:inline">{c.parameter}</span>
                    </span>
                    <span className="text-right tabular-nums sm:whitespace-nowrap">
                      {kind !== 'baseline' && !added && (
                        <>
                          <span className="text-muted line-through decoration-faint/60">
                            {formatValue(c.before, c.unit, c.scopeLabel)}
                          </span>
                          <span className="mx-1.5 text-faint sm:mx-2">→</span>
                        </>
                      )}
                      <span className="font-bold">{formatValue(c.after, c.unit, c.scopeLabel)}</span>
                      {delta && <span className="ml-1.5 text-xs text-muted sm:hidden">{delta}</span>}
                    </span>
                    <span className="hidden text-right text-xs whitespace-nowrap text-muted tabular-nums sm:block">
                      {delta}
                    </span>
                  </li>
                )
              })}
            </ul>
            {hidden > 0 && (
              <button
                type="button"
                onClick={() => setAll(true)}
                className="w-full border-t border-line-soft px-4 py-2 text-left text-xs font-bold text-accent hover:bg-hover"
              >
                Show {hidden} more
              </button>
            )}
          </div>
        ) : (
          <button
            type="button"
            onClick={() => setOpen(true)}
            className="w-full border-t border-line-soft px-4 py-2 text-left text-xs font-bold text-accent hover:bg-hover"
          >
            Show the {set.changes.length} values
          </button>
        )}
      </div>
    </div>
  )
}

function SourcePill({ source }: { source: string }) {
  const kind = sourceKind(source)
  const tone = {
    panel: 'bg-hover text-ink',
    host: 'bg-warning-bg text-warning',
    baseline: 'bg-hover text-muted',
  }[kind]
  return (
    <span className={`rounded-full px-2 py-0.5 text-xs font-bold ${tone}`}>{sourceLabel(source)}</span>
  )
}

const SHIP: Record<ShipState, { label: string; tone: string; title: string }> = {
  local: {
    label: 'on this machine',
    tone: 'text-faint',
    title: "This bot doesn't send its log to Textile",
  },
  shipped: {
    label: '✓ sent',
    tone: 'text-success',
    title: 'Textile acknowledged this record',
  },
  waiting: {
    label: 'sending…',
    tone: 'text-warning',
    title: 'The bot sends new records every minute',
  },
  refused: {
    label: 'refused',
    tone: 'text-danger font-bold',
    title: 'Textile refused this record. It stays in the local log.',
  },
}

function ShipPill({ state }: { state: ShipState }) {
  const s = SHIP[state]
  return (
    <span title={s.title} className={`text-xs ${s.tone}`}>
      {s.label}
    </span>
  )
}

/** Hand a file to the browser's download. The one side effect behind Export. */
function downloadText(filename: string, text: string) {
  const url = URL.createObjectURL(new Blob([text], { type: 'text/csv;charset=utf-8' }))
  const a = document.createElement('a')
  a.href = url
  a.download = filename
  a.click()
  // Revoked a moment later: some browsers start the download after click returns.
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}

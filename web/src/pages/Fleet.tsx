import { useCallback, useEffect, useState } from 'react'
import { botLabel, botPath } from '../botRoutes'
import { Link, useLocation } from 'react-router-dom'
import { ApiError, api } from '../api'
import {
  Banner,
  Button,
  Card,
  Empty,
  ErrorState,
  Loading,
  StatePill,
  Tag,
} from '../components/ui'
import { formatUsd } from '../format'
import {
  arrangeFleet,
  rankFleet,
  readFleetOrder,
  saveFleetOrder,
  type RowTotal,
} from '../fleetOrder'
import { totalUsd, unpricedSymbols } from '../funding'
import type { Bot, Fleet as FleetData, UpdatesStatus } from '../types'

/** How often the list refreshes itself, so a bot that dies is visible without a reload. */
const POLL_MS = 5000

/** When to rank without the wallets still unread: just past the server's
 * six-second chain budget, so a read that is coming has had its chance. */
const FIRST_READ_CAP_MS = 8000

export default function Fleet() {
  const [data, setData] = useState<FleetData | null>(null)
  const [updates, setUpdates] = useState<UpdatesStatus | null>(null)
  const [error, setError] = useState<string | null>(null)
  // A bot that was just removed or created redirects here with what happened, so
  // the confirmation isn't lost with the page it was shown on.
  const handoff = (useLocation().state as { note?: string } | null)?.note ?? null
  const [note, setNote] = useState<string | null>(handoff)
  // Dollar value per bot, read here rather than per row because the order
  // of the list depends on it. Each bot is its own request, so one slow
  // chain leaves that bot's value unknown rather than stalling the rest.
  const [values, setValues] = useState<Record<string, RowTotal>>({})
  // The order rows render in, read once and kept for the visit: rows update in
  // place and never move. See fleetOrder.ts.
  const [order] = useState(readFleetOrder)
  // Bots whose first wallet read came back, either way. The next visit's order
  // is only saved once all have, or the cap has passed, so a wallet still being
  // read isn't saved at the bottom.
  const [settled, setSettled] = useState<ReadonlySet<string>>(new Set())
  const [capped, setCapped] = useState(false)

  const load = useCallback(async () => {
    try {
      setData(await api.fleet())
      setError(null)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    }
  }, [])

  const loadUpdates = useCallback(async () => {
    try {
      setUpdates(await api.updates())
    } catch {
      setUpdates(null)
    }
  }, [])

  useEffect(() => {
    void load()
    void loadUpdates()
    const timer = setInterval(() => void load(), POLL_MS)
    return () => clearInterval(timer)
  }, [load, loadUpdates])

  const botNames = (data?.bots ?? [])
    .filter((b) => b.config)
    .map((b) => b.name)
    .join('\n')
  useEffect(() => {
    const names = botNames ? botNames.split('\n') : []
    if (names.length === 0) return
    let cancelled = false
    const markSettled = (name: string) =>
      setSettled((s) => (s.has(name) ? s : new Set([...s, name])))
    const read = () => {
      for (const name of names) {
        void api
          .funding(name)
          .then((funding) => {
            if (cancelled) return
            setValues((v) => ({
              ...v,
              [name]: {
                usd: totalUsd(funding),
                unpriced: unpricedSymbols(funding),
              },
            }))
            markSettled(name)
          })
          .catch(() => {
            // Ranked as unknown: bottom of its band.
            if (!cancelled) markSettled(name)
          })
      }
    }
    read()
    const timer = window.setInterval(read, VALUE_POLL_MS)
    const cap = window.setTimeout(() => setCapped(true), FIRST_READ_CAP_MS)
    return () => {
      cancelled = true
      clearInterval(timer)
      clearTimeout(cap)
    }
  }, [botNames])

  // Rank for the next visit, and keep re-ranking as reads land so the saved
  // order is the latest one. Never touches what this visit renders.
  const firstRoundDone =
    capped || (botNames ? botNames.split('\n') : []).every((n) => settled.has(n))
  useEffect(() => {
    if (data && firstRoundDone) saveFleetOrder(rankFleet(data.bots, values))
  }, [data, values, firstRoundDone])

  if (!data && error) return <ErrorState error={error} onRetry={() => void load()} />
  if (!data) return <Loading what="the fleet" />

  const behind = new Set(
    (updates?.bots ?? []).filter((b) => b.updateAvailable).map((b) => b.name),
  )

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between gap-4">
        <h1 className="text-xl font-bold">
          {data.bots.length} {data.bots.length === 1 ? 'bot' : 'bots'}
        </h1>
        <Link to="/add">
          <Button variant="primary">Add corridor</Button>
        </Link>
      </div>

      {error && <Banner tone="danger" onDismiss={() => setError(null)}>{error}</Banner>}
      {note && <Banner tone="info" onDismiss={() => setNote(null)}>{note}</Banner>}
      {behind.size > 0 && (
        <Banner tone="info">
          {behind.size} {behind.size === 1 ? 'bot has' : 'bots have'} a stitch image update
          available. Open a bot and click Update.
        </Banner>
      )}

      {/* The first-time screen. It sits under a button labelled Add corridor,
          and the operator has never heard the word bot, so it leads with what
          that button does rather than with what is missing. */}
      {data.bots.length === 0 ? (
        <Empty title="Nothing running here yet">
          <p>
            Add a corridor to set up your first bot. Or point{' '}
            <code>STITCH_PANEL_BOTS_DIR</code> at the directory holding your
            existing configs. The panel currently reads{' '}
            <code>{data.botsDir}</code>.
          </p>
        </Empty>
      ) : (
        <ul className="space-y-3">
          {arrangeFleet(data.bots, order).map((bot) => (
            <li key={bot.name}>
              <BotRow
                bot={bot}
                value={values[bot.name]}
                updateAvailable={
                  behind.has(bot.name) && !bot.canMigrate && bot.layout !== 'flat-files'
                }
              />
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}

function BotRow({
  bot,
  value,
  updateAvailable,
}: {
  bot: Bot
  /** The wallet's worth, undefined while unread. */
  value: RowTotal | undefined
  updateAvailable: boolean
}) {
  const blocking = bot.warnings.filter((w) => w.blocksEditing)
  const advisory = bot.warnings.filter((w) => !w.blocksEditing)
  const pairs = bot.config?.pairs ?? []
  const network = bot.config?.networkLabel ?? (bot.config ? `chain ${bot.config.chainId}` : null)

  // The whole row is the link: a fleet is for picking a bot, and every
  // action lives on the bot's page. No buttons here to compete with it.
  return (
    <Card className="!p-0">
      <Link
        to={botPath(bot.name)}
        className="flex flex-col gap-3 p-4 no-underline hover:bg-hover sm:flex-row sm:items-center"
      >
        <div className="flex min-w-0 flex-1 flex-col gap-1.5">
          <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
            <span className="font-bold text-ink">{botLabel(bot)}</span>
            {bot.displayName && (
              <span className="font-mono text-xs text-faint">{bot.name}</span>
            )}
            <StatePill state={bot.state} status={bot.status} venue={bot.config?.venue} />
            {updateAvailable && <Tag>update available</Tag>}
            {!bot.container && <Tag>no container</Tag>}
          </div>
          {/* One chip per corridor, so three corridors read as three pairs and
              wrap to a second line rather than being counted. */}
          {(pairs.length > 0 || network) && (
            <div className="flex flex-wrap items-center gap-1.5 text-sm text-muted">
              {pairs.map((pair, i) => (
                <span
                  key={`${pair}-${i}`}
                  className="rounded-md border border-line-soft px-1.5 py-0.5 text-xs"
                >
                  {pair}
                </span>
              ))}
              {network && <span className="text-xs text-faint">on {network}</span>}
            </div>
          )}
        </div>
        {bot.config && <RowValue value={value} />}
      </Link>

      {(blocking.length > 0 || advisory.length > 0) && (
        <div className="space-y-2 px-4 pb-4">
          {blocking.map((w) => (
            <Banner key={w.kind} tone="danger">
              {w.message}
            </Banner>
          ))}
          {advisory.map((w) => (
            <Banner key={w.kind} tone="warning">
              {w.message}
              {w.kind === 'ledgerNotPersisted' && bot.canMigrate && (
                <>
                  {' '}
                  <Link
                    to={botPath(bot.name)}
                    className="font-bold underline"
                  >
                    Fix it
                  </Link>
                </>
              )}
            </Banner>
          ))}
        </div>
      )}
    </Card>
  )
}

const VALUE_POLL_MS = 5000

/** The same figure and the same "+ unpriced" caveat as the bot page's header,
 * so a wallet holding an unpriceable balance never reads as a few dollars. */
function RowValue({ value }: { value: RowTotal | undefined }) {
  const usd = value?.usd ?? null
  const unpriced = value?.unpriced ?? []
  return (
    <span className="shrink-0 text-right sm:ml-auto">
      <span
        className="text-base font-bold tabular-nums text-ink"
        title={value === undefined ? 'Reading the wallet' : 'Everything in the wallet, in dollars'}
      >
        {usd === null ? <span className="text-faint">—</span> : formatUsd(usd)}
      </span>
      {unpriced.length > 0 && (
        <span className="ml-2 text-xs text-warning" title={`Not priced: ${unpriced.join(', ')}`}>
          + unpriced {unpriced.join(', ')}
        </span>
      )}
    </span>
  )
}

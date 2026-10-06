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
import { fundsFromVault, textileVaultUrl } from '../capital'
import { useBalancesHidden } from '../balancePrivacy'
import PrivateBalance, { Dots } from '../components/PrivateBalance'
import { capitalUnpricedSymbols, capitalUsd } from '../funding'
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
  // of the list depends on it. A vault maker is worth its vault, not the
  // gas on the key that signs for it. Each bot is its own request, so one slow
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
  const [balancesHidden, toggleBalances] = useBalancesHidden()

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
                usd: capitalUsd(funding),
                unpriced: capitalUnpricedSymbols(funding),
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
                balancesHidden={balancesHidden}
                onToggleBalances={toggleBalances}
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
  balancesHidden,
  onToggleBalances,
  updateAvailable,
}: {
  bot: Bot
  /** The capital's worth, undefined while unread. */
  value: RowTotal | undefined
  balancesHidden: boolean
  onToggleBalances: () => void
  updateAvailable: boolean
}) {
  const blocking = bot.warnings.filter((w) => w.blocksEditing)
  const advisory = bot.warnings.filter((w) => !w.blocksEditing)
  const pairs = bot.config?.pairs ?? []
  const network = bot.config?.networkLabel ?? (bot.config ? `chain ${bot.config.chainId}` : null)
  const vaultUrl = textileVaultUrl(bot.config)

  // The whole row opens the bot: a fleet is for picking a bot, and every
  // action lives on the bot's page. The name's link stretches over the row
  // (`after:inset-0`) rather than wrapping it, so the vault button and the
  // balance can sit above it with their own clicks; an <a> can't nest inside
  // another, nor a <button> inside an <a>.
  //
  // `overflow-hidden` clips the row's hover fill to the card's rounded corners.
  return (
    <Card className="overflow-hidden !p-0">
      <div className="relative flex flex-col gap-3 p-4 hover:bg-hover sm:flex-row sm:items-center">
        <div className="flex min-w-0 flex-1 flex-col gap-1.5">
          <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
            <Link
              to={botPath(bot.name)}
              className="font-bold text-ink no-underline after:absolute after:inset-0"
            >
              {botLabel(bot)}
            </Link>
            {bot.displayName && (
              <span className="font-mono text-xs text-faint">{bot.name}</span>
            )}
            <StatePill state={bot.state} status={bot.status} venue={bot.config?.venue} />
            {vaultUrl && <VaultButton href={vaultUrl} />}
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
        {bot.config && (
          <RowValue
            value={value}
            vaulted={fundsFromVault(bot.config)}
            hidden={balancesHidden}
            onToggle={onToggleBalances}
          />
        )}
      </div>

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

/** Opens the bot's vault on the Textile app. Accent-tinted so it reads as a
 * link, not one of the row's grey status tags. `relative z-10` lifts it over
 * the row's stretched link so the click lands here, not on the bot. */
function VaultButton({ href }: { href: string }) {
  return (
    <a
      href={href}
      target="_blank"
      rel="noreferrer"
      title="Open this vault on Textile"
      className="relative z-10 rounded-md bg-accent-tint px-1.5 py-0.5 text-xs font-bold text-accent no-underline hover:bg-accent hover:text-on-accent"
    >
      vault ↗
    </a>
  )
}

const VALUE_POLL_MS = 5000

/** The capital the bot quotes against, with the same "+ unpriced" caveat as
 * the bot page's header, so a wallet holding an unpriceable balance never reads
 * as a few dollars. For a vault maker that is the vault's quotable inventory. */
function RowValue({
  value,
  vaulted,
  hidden,
  onToggle,
}: {
  value: RowTotal | undefined
  vaulted: boolean
  /** Masks the amount. A dash stays a dash: "not read" isn't a balance. */
  hidden: boolean
  onToggle: () => void
}) {
  const usd = value?.usd ?? null
  const unpriced = value?.unpriced ?? []
  return (
    <span className="shrink-0 text-right sm:ml-auto">
      {usd === null ? (
        <span
          className="text-base font-bold text-faint"
          title={rowValueTitle(value === undefined, vaulted)}
        >
          —
        </span>
      ) : (
        <PrivateBalance
          hidden={hidden}
          onToggle={onToggle}
          masked={<Dots size="md" />}
          title={rowValueTitle(false, vaulted)}
          // Above the row's stretched link, so the click masks rather than
          // opening the bot.
          className="relative z-10"
        >
          <span className="text-base font-bold tabular-nums">{formatUsd(usd)}</span>
        </PrivateBalance>
      )}
      {unpriced.length > 0 && (
        <span className="ml-2 text-xs text-warning" title={`Not priced: ${unpriced.join(', ')}`}>
          + unpriced {unpriced.join(', ')}
        </span>
      )}
    </span>
  )
}

function rowValueTitle(reading: boolean, vaulted: boolean): string {
  if (reading) return vaulted ? 'Reading the vault' : 'Reading the wallet'
  return vaulted ? 'What the vault can quote, in dollars' : 'Everything in the wallet, in dollars'
}

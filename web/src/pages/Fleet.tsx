import { useCallback, useEffect, useState } from 'react'
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
import { shortAddress, shortImage } from '../format'
import type { Bot, Fleet as FleetData, UpdatesStatus } from '../types'

/** How often the list refreshes itself, so a bot that dies is visible without a reload. */
const POLL_MS = 5000

export default function Fleet() {
  const [data, setData] = useState<FleetData | null>(null)
  const [updates, setUpdates] = useState<UpdatesStatus | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  // A bot that was just removed or created redirects here with what happened, so
  // the confirmation isn't lost with the page it was shown on.
  const handoff = (useLocation().state as { note?: string } | null)?.note ?? null
  const [note, setNote] = useState<string | null>(handoff)

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

  async function act(name: string, what: 'start' | 'stop' | 'update') {
    if (what === 'update') {
      const bot = data?.bots.find((b) => b.name === name)
      // Recreate drops an in-container nonce ledger. Flat-layout bots must migrate
      // first — the detail page already says so; don't offer a quieter path here.
      if (bot && (bot.canMigrate || bot.layout === 'flat-files')) {
        setError(
          `${name} still uses the flat layout. Open it and migrate before updating, or recreating loses the slot-nonce ledger and live orders can collide.`,
        )
        return
      }
      if (
        !window.confirm(
          `Update ${name} to ${updates?.bot.targetImage ?? 'the panel bot image'}?\n\nThe container is recreated (brief gap in quoting). Config and key stay.`,
        )
      ) {
        return
      }
    }
    setBusy(`${name}:${what}`)
    setNote(null)
    try {
      const res = what === 'update' ? await api.updateBot(name) : await api[what](name)
      if (res.message) setNote(res.message)
      await load()
      await loadUpdates()
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(null)
    }
  }

  if (!data && error) return <ErrorState error={error} onRetry={() => void load()} />
  if (!data) return <Loading what="the fleet" />

  const behind = new Set(
    (updates?.bots ?? []).filter((b) => b.updateAvailable).map((b) => b.name),
  )
  const canUpdate = new Set(
    (updates?.bots ?? []).filter((b) => b.canUpdate).map((b) => b.name),
  )

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between gap-4">
        <h1 className="text-xl font-bold">
          {data.bots.length} {data.bots.length === 1 ? 'bot' : 'bots'}
        </h1>
        <Link to="/add">
          <Button variant="primary">Add a bot</Button>
        </Link>
      </div>

      {error && <Banner tone="danger" onDismiss={() => setError(null)}>{error}</Banner>}
      {note && <Banner tone="info" onDismiss={() => setNote(null)}>{note}</Banner>}
      {behind.size > 0 && (
        <Banner tone="info">
          {behind.size} {behind.size === 1 ? 'bot has' : 'bots have'} a stitch image update
          available. Open a bot and click Update, or use Update on the row.
        </Banner>
      )}

      {data.bots.length === 0 ? (
        <Empty title="No bots on this host yet">
          <p>
            Add one, or point <code>STITCH_PANEL_BOTS_DIR</code> at the directory
            holding your existing configs. The panel currently reads{' '}
            <code>{data.botsDir}</code>.
          </p>
        </Empty>
      ) : (
        <ul className="space-y-3">
          {[...data.bots]
            .sort((a, b) => a.name.localeCompare(b.name))
            .map((bot) => (
            <li key={bot.name}>
              <BotRow
                bot={bot}
                busy={busy}
                updateAvailable={
                  behind.has(bot.name) && !bot.canMigrate && bot.layout !== 'flat-files'
                }
                canUpdate={
                  canUpdate.has(bot.name) && !bot.canMigrate && bot.layout !== 'flat-files'
                }
                onAct={act}
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
  busy,
  updateAvailable,
  canUpdate,
  onAct,
}: {
  bot: Bot
  busy: string | null
  updateAvailable: boolean
  canUpdate: boolean
  onAct: (name: string, what: 'start' | 'stop' | 'update') => void
}) {
  const blocking = bot.warnings.filter((w) => w.blocksEditing)
  const advisory = bot.warnings.filter((w) => !w.blocksEditing)

  return (
    <Card className="!p-4">
      <div className="flex flex-col gap-3 sm:flex-row sm:flex-wrap sm:items-center">
        <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
          <Link to={`/bots/${encodeURIComponent(bot.name)}`} className="font-bold">
            {bot.name}
          </Link>
          <StatePill state={bot.state} status={bot.status} />
          {bot.config?.corridorLabel && (
            <span className="text-sm text-muted">{bot.config.corridorLabel}</span>
          )}
          {updateAvailable && <Tag>update available</Tag>}
        </div>
        <div
          className={`grid w-full gap-2 sm:ml-auto sm:flex sm:w-auto sm:flex-wrap sm:items-center [&_button]:w-full [&_button]:justify-center sm:[&_button]:w-auto ${
            bot.container && canUpdate ? 'grid-cols-2' : 'grid-cols-1'
          }`}
        >
          {bot.container ? (
            <>
              {bot.canStop ? (
                <Button
                  busy={busy === `${bot.name}:stop`}
                  onClick={() => onAct(bot.name, 'stop')}
                  title="Graceful stop: the bot finishes its tick, then exits"
                >
                  Stop
                </Button>
              ) : (
                <Button
                  busy={busy === `${bot.name}:start`}
                  onClick={() => onAct(bot.name, 'start')}
                >
                  Start
                </Button>
              )}
              {canUpdate && (
                <Button
                  variant="primary"
                  busy={busy === `${bot.name}:update`}
                  onClick={() => onAct(bot.name, 'update')}
                >
                  Update
                </Button>
              )}
            </>
          ) : (
            <Tag>no container</Tag>
          )}
        </div>
      </div>

      <div className="mt-3 flex flex-wrap items-center gap-2 text-xs text-faint">
        <Tag>{bot.origin}</Tag>
        <Tag>{bot.layout}</Tag>
        {bot.config && <Tag>chain {bot.config.chainId}</Tag>}
        {bot.config?.operatorAddress && (
          <Tag>
            <span title={bot.config.operatorAddress}>
              {shortAddress(bot.config.operatorAddress)}
            </span>
          </Tag>
        )}
        {bot.config && <Tag>{bot.config.signer}</Tag>}
        <span className="ml-auto font-mono" title={bot.image ?? undefined}>
          {shortImage(bot.image)}
        </span>
      </div>

      {(blocking.length > 0 || advisory.length > 0) && (
        <div className="mt-3 space-y-2">
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
                    to={`/bots/${encodeURIComponent(bot.name)}`}
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

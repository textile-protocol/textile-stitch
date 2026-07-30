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
import type { Bot, Fleet as FleetData } from '../types'

/** How often the list refreshes itself, so a bot that dies is visible without a reload. */
const POLL_MS = 5000

export default function Fleet() {
  const [data, setData] = useState<FleetData | null>(null)
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

  useEffect(() => {
    void load()
    const timer = setInterval(() => void load(), POLL_MS)
    return () => clearInterval(timer)
  }, [load])

  async function act(name: string, what: 'start' | 'stop' | 'restart') {
    setBusy(`${name}:${what}`)
    setNote(null)
    try {
      const res = await api[what](name)
      if (res.message) setNote(res.message)
      await load()
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(null)
    }
  }

  if (!data && error) return <ErrorState error={error} onRetry={() => void load()} />
  if (!data) return <Loading what="the fleet" />

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
          {data.bots.map((bot) => (
            <li key={bot.name}>
              <BotRow bot={bot} busy={busy} onAct={act} />
            </li>
          ))}
        </ul>
      )}

      <p className="text-xs text-faint">
        New bots run <code>{shortImage(data.botImage)}</code>. Configs live in{' '}
        <code>{data.botsDir}</code>.
      </p>
    </div>
  )
}

function BotRow({
  bot,
  busy,
  onAct,
}: {
  bot: Bot
  busy: string | null
  onAct: (name: string, what: 'start' | 'stop' | 'restart') => void
}) {
  const blocking = bot.warnings.filter((w) => w.blocksEditing)
  const advisory = bot.warnings.filter((w) => !w.blocksEditing)

  return (
    <Card className="!p-4">
      <div className="flex flex-wrap items-center gap-3">
        <Link to={`/bots/${encodeURIComponent(bot.name)}`} className="font-bold">
          {bot.name}
        </Link>
        <StatePill state={bot.state} status={bot.status} />
        {bot.config?.corridorLabel && (
          <span className="text-sm text-muted">{bot.config.corridorLabel}</span>
        )}
        <div className="ml-auto flex items-center gap-2">
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
              {/*
                Same precondition as Stop: there has to be a process to bounce.
                `docker restart` on a stopped container *starts* it, so an enabled
                Restart next to Start is a second Start wearing the wrong label.
              */}
              <Button
                busy={busy === `${bot.name}:restart`}
                disabled={!bot.canStop}
                onClick={() => onAct(bot.name, 'restart')}
                title={
                  bot.canStop
                    ? 'Stop and start it again, with the full tick grace period'
                    : `${bot.name} is ${bot.state} — there is nothing to restart. Use Start.`
                }
              >
                Restart
              </Button>
            </>
          ) : (
            <Tag>no container</Tag>
          )}
          <Link to={`/bots/${encodeURIComponent(bot.name)}`}>
            <Button variant="ghost">Open</Button>
          </Link>
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
        <span className="ml-auto">{shortImage(bot.image)}</span>
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

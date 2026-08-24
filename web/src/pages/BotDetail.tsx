import { useCallback, useEffect, useState } from 'react'
import { Link, useLocation, useNavigate, useParams, useSearchParams } from 'react-router-dom'
import { BOT_TABS, TAB_LABEL, botPath, parseBotTab, type BotTab } from '../botRoutes'
import { ApiError, api } from '../api'
import {
  Banner,
  Button,
  Card,
  ErrorState,
  Loading,
  StatePill,
  Tag,
  Warnings,
} from '../components/ui'
import BotSwitcher from '../components/BotSwitcher'
import ComposeExportLink from '../components/ComposeExportLink'
import LogViewer from '../components/LogViewer'
import OneShotRunner from '../components/OneShotRunner'
import Permit2Allowances from '../components/Permit2Allowances'
import RawConfigEditor from '../components/RawConfigEditor'
import SettingsForm from '../components/SettingsForm'
import StitchDashboardEmbed from '../components/StitchDashboardEmbed'
import VersionRollback from '../components/VersionRollback'
import { formatTimestamp, shortAddress, shortImage } from '../format'
import { confirmRemovePlan } from '../removeBot'
import type { Bot, ConfigBody, MigrationResult, UpdatesStatus } from '../types'

export default function BotDetail() {
  const { name = '' } = useParams()
  const navigate = useNavigate()
  const [searchParams, setSearchParams] = useSearchParams()
  const [bot, setBot] = useState<Bot | null>(null)
  const [error, setError] = useState<string | null>(null)
  // The wizard redirects here with what it just did, so its confirmation survives
  // the navigation.
  const handoff = useLocation().state as {
    note?: string
    needsPermit2?: boolean
  } | null
  const [note, setNote] = useState<string | null>(handoff?.note ?? null)
  // Bumped after a successful approve so the allowance rows re-read the chain
  // instead of still showing what was missing a moment ago.
  const [approvedAt, setApprovedAt] = useState(0)
  const [showPermit2Banner, setShowPermit2Banner] = useState(
    () => !!handoff?.needsPermit2,
  )
  const [busy, setBusy] = useState<string | null>(null)
  // After create, land on Tools so Approve allowances is the next obvious step.
  // Tab lives in `?tab=` so switching bots from the title keeps the same section.
  const fallbackTab: BotTab = handoff?.needsPermit2 ? 'tools' : 'settings'
  const tab = parseBotTab(searchParams.get('tab'), fallbackTab)
  const [updates, setUpdates] = useState<UpdatesStatus | null>(null)

  useEffect(() => {
    const raw = searchParams.get('tab')
    if (raw === tab) return
    setSearchParams(
      (prev) => {
        const next = new URLSearchParams(prev)
        next.set('tab', tab)
        return next
      },
      { replace: true },
    )
  }, [searchParams, setSearchParams, tab])

  const load = useCallback(async (signal?: { cancelled: boolean }) => {
    try {
      const next = await api.bot(name)
      if (signal?.cancelled) return
      setBot(next)
      setError(null)
    } catch (e) {
      if (signal?.cancelled) return
      setError(e instanceof ApiError ? e.message : String(e))
    }
  }, [name])

  const loadUpdates = useCallback(async () => {
    try {
      setUpdates(await api.updates())
    } catch {
      // Offline registry checks are soft — don't drown the detail page.
      setUpdates(null)
    }
  }, [])

  useEffect(() => {
    const signal = { cancelled: false }
    void load(signal)
    void loadUpdates()
    return () => {
      signal.cancelled = true
    }
  }, [load, loadUpdates])

  const botUpdate = updates?.bots.find((b) => b.name === name)
  const updateAvailable = botUpdate?.updateAvailable ?? false
  // Pins keep an Update button even when no newer digest was detected.
  const canUpdate = botUpdate?.canUpdate ?? updateAvailable

  async function act(
    what: 'start' | 'stop' | 'restart' | 'recreate' | 'update',
    target = name,
  ) {
    if (
      what === 'recreate' &&
      !window.confirm(
        `Recreate ${target}'s container? It's replaced with a fresh one on the current image, using the same config directory.`,
      )
    ) {
      return
    }
    if (what === 'update') {
      if (bot?.canMigrate || bot?.layout === 'flat-files') {
        setError(
          `${target} still uses the flat layout. Migrate first so the nonce ledger isn't lost on recreate.`,
        )
        return
      }
      if (
        !window.confirm(
          `Update ${target} to ${updates?.bot.targetImage ?? 'the panel bot image'}?\n\nThe container is recreated (brief gap in quoting). Config and key stay.`,
        )
      ) {
        return
      }
    }
    setBusy(what)
    setError(null)
    try {
      const res =
        what === 'update' ? await api.updateBot(target) : await api[what](target)
      // Stopping a sibling to unblock approval returns *that* bot. Keep this
      // page on the selected one and re-read so canApprove can flip.
      setBot(target === name ? res.bot : await api.bot(name))
      setNote(res.message)
      void loadUpdates()
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(null)
    }
  }

  async function remove() {
    const plan = confirmRemovePlan({
      name,
      hasContainer: !!bot?.container,
    })
    if (!plan) return

    setBusy('remove')
    setError(null)
    try {
      const res = await api.remove(name, plan.deleteConfig)
      navigate('/', { state: { note: res.message } })
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
      setBusy(null)
    }
  }

  // Never paint the previous bot while the URL name has moved on. The
  // dashboard iframe in particular will keep the old document if we do.
  if (bot && bot.name !== name) {
    if (error) return <ErrorState error={error} onRetry={() => void load()} />
    return <Loading what={name} />
  }
  if (!bot && error) return <ErrorState error={error} onRetry={() => void load()} />
  if (!bot) return <Loading what={name} />

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-3">
        <Link to="/" className="text-sm text-muted hover:text-ink">
          ← Fleet
        </Link>
        <BotSwitcher name={bot.name} />
        <StatePill state={bot.state} status={bot.status} />
        {bot.config?.corridorLabel && (
          <span className="text-sm text-muted">{bot.config.corridorLabel}</span>
        )}
        {updateAvailable && <Tag>update available</Tag>}
      </div>

      {canUpdate &&
        (bot.canMigrate || bot.layout === 'flat-files' ? (
          <Banner tone="warning">
            {updateAvailable ? (
              <>
                A newer stitch image is available (
                {shortImage(updates?.bot.targetImage ?? null)}), but this bot still
                uses the flat layout. Migrate first so updating doesn't lose the
                slot-nonce ledger.
              </>
            ) : (
              <>
                This bot is on a pinned image. Migrate first so updating to{' '}
                {shortImage(updates?.bot.targetImage ?? null)} doesn't lose the
                slot-nonce ledger.
              </>
            )}
          </Banner>
        ) : updateAvailable ? (
          <Banner tone="info">
            A newer stitch image is available ({shortImage(updates?.bot.targetImage ?? null)}
            ). Update recreates this bot on that image; config stays on disk.
          </Banner>
        ) : (
          <Banner tone="info">
            This bot is on a pinned image (
            {shortImage(bot.image)}). Update moves it to{' '}
            {shortImage(updates?.bot.targetImage ?? null)}; config stays on disk.
          </Banner>
        ))}

      {error && (
        <Banner tone="danger" onDismiss={() => setError(null)}>
          {error}
        </Banner>
      )}
      {note && (
        <Banner tone="success" onDismiss={() => setNote(null)}>
          {note}
        </Banner>
      )}

      {showPermit2Banner && (
        <Banner tone="warning" onDismiss={() => setShowPermit2Banner(false)}>
          <div className="space-y-2">
            <p>
              <strong>Permit2 approval required.</strong> This bot cannot trade
              until its operator wallet approves Permit2 for each input token.
              That is a one-time on-chain step and needs a little native gas on
              the wallet (for the approval transactions).
            </p>
            <p>
              Open{' '}
              <Link
                to={botPath(name, 'tools')}
                replace
                className="font-bold underline hover:no-underline"
              >
                Tools → Approve allowances
              </Link>
              , then dry-run, then Start.
            </p>
          </div>
        </Banner>
      )}

      <Card>
        {/*
          On narrow screens a single flex+ml-auto row wraps badly: Update stays
          left and Remove jumps to the far right of the next line. Lifecycle
          actions share a 2-col grid on mobile; Remove sits full-width under
          them. From sm up, everything is one wrapping row with Remove pushed
          to the end.
        */}
        <div className="flex flex-col gap-2 sm:flex-row sm:flex-wrap sm:items-center">
          <div className="grid grid-cols-2 gap-2 sm:flex sm:flex-wrap sm:items-center [&_button]:w-full sm:[&_button]:w-auto">
            {bot.container ? (
              <>
                {bot.canStop ? (
                  <Button
                    busy={busy === 'stop'}
                    onClick={() => void act('stop')}
                    title="Sends SIGTERM and waits, so the bot finishes its tick and cancels cleanly"
                  >
                    Stop
                  </Button>
                ) : (
                  <Button busy={busy === 'start'} onClick={() => void act('start')}>
                    Start
                  </Button>
                )}
                {/*
                  Same precondition as Stop: there has to be a process to bounce.
                  `docker restart` on a stopped container starts it, so an enabled
                  Restart next to Start is a second Start wearing the wrong label.
                */}
                <Button
                  busy={busy === 'restart'}
                  disabled={!bot.canStop}
                  onClick={() => void act('restart')}
                  title={
                    bot.canStop
                      ? 'Stop and start it again, with the full tick grace period'
                      : `${bot.name} is ${bot.state} — there is nothing to restart. Use Start.`
                  }
                >
                  Restart
                </Button>
                <Button busy={busy === 'recreate'} onClick={() => void act('recreate')}>
                  Recreate
                </Button>
                {canUpdate && !bot.canMigrate && bot.layout !== 'flat-files' && (
                  <Button
                    variant="primary"
                    busy={busy === 'update'}
                    onClick={() => void act('update')}
                    title={`Pull ${updates?.bot.targetImage} and recreate this bot on it`}
                  >
                    Update
                  </Button>
                )}
              </>
            ) : (
              <>
                <Tag>no container</Tag>
                {/*
                  Config is on disk but no container: the wizard failed mid-create, or
                  someone removed the container and kept the files. Recreate is the
                  only recovery — Add Bot conflicts with the existing directory.
                */}
                <Button
                  busy={busy === 'recreate'}
                  onClick={() => void act('recreate')}
                  title="Build a container from the config already on disk"
                >
                  Recreate
                </Button>
              </>
            )}
          </div>
          <Button
            variant="danger"
            busy={busy === 'remove'}
            className="w-full sm:ml-auto sm:w-auto"
            onClick={() => void remove()}
            title={
              bot.container
                ? 'Delete the container, config, and private key — gone from the fleet'
                : 'Delete config and private key — gone from the fleet'
            }
          >
            {bot.container ? 'Remove' : 'Delete'}
          </Button>
        </div>

        <dl className="mt-4 grid gap-3 text-sm sm:grid-cols-2 lg:grid-cols-4">
          <Detail label="Origin">{bot.origin}</Detail>
          <Detail label="Layout">{bot.layout}</Detail>
          <Detail label="Image">
            <span className="font-mono" title={bot.image ?? undefined}>
              {shortImage(bot.image)}
            </span>
          </Detail>
          <Detail label="Created">{formatTimestamp(bot.createdUnix)}</Detail>
          <Detail label="Chain">{bot.config ? bot.config.chainId : '—'}</Detail>
          <Detail label="Pools">{bot.config ? bot.config.pools : '—'}</Detail>
          <Detail label="Signer">{bot.config?.signer ?? '—'}</Detail>
          <Detail label="Operator">
            <OperatorAddress config={bot.config} />
          </Detail>
        </dl>
      </Card>

      {bot.warnings.length > 0 && (
        <Card title="Warnings">
          <Warnings warnings={bot.warnings} />
        </Card>
      )}

      <MigrationPrompt bot={bot} onMigrated={setBot} />

      {/*
        Five labels don't fit a phone width. Scroll horizontally in a clipped
        strip (not the page) so they stay one line; a right-edge fade hints
        that more tabs sit off-screen.
      */}
      <div className="relative">
        <nav
          className="flex flex-nowrap gap-x-0.5 overflow-x-auto border-b border-line-soft [scrollbar-width:none] [-ms-overflow-style:none] sm:gap-x-1 [&::-webkit-scrollbar]:hidden"
          aria-label="Bot sections"
        >
          {BOT_TABS.map((t) => (
            <Link
              key={t}
              to={botPath(name, t)}
              replace
              aria-current={tab === t ? 'page' : undefined}
              className={`-mb-px shrink-0 border-b-2 px-2.5 py-2 text-xs font-bold transition sm:px-3 sm:text-sm ${
                tab === t
                  ? 'border-accent text-ink'
                  : 'border-transparent text-muted hover:text-ink'
              }`}
            >
              {TAB_LABEL[t]}
            </Link>
          ))}
        </nav>
        <div
          aria-hidden
          className="pointer-events-none absolute inset-y-0 right-0 w-8 bg-gradient-to-l from-canvas to-transparent sm:hidden"
        />
      </div>

      {tab === 'settings' &&
        (bot.editable ? (
          <SettingsForm
            bot={bot}
            onSaved={(message) => {
              setNote(message)
              void load()
            }}
          />
        ) : (
          <Banner tone="warning">
            The panel can see this bot but won't write its config. The warnings above
            say why.
          </Banner>
        ))}

      {tab === 'config' && (
        <Card>
          <RawConfigEditor
            bot={bot.name}
            running={bot.running}
            onSaved={(message) => {
              setNote(message)
              void load()
            }}
          />
        </Card>
      )}

      {tab === 'logs' && (
        <Card title="Logs">
          <LogViewer bot={bot.name} />
        </Card>
      )}

      {tab === 'tools' && (
        <>
          {/*
            Above the runs: what needs approving is the thing an operator came
            here to find out, and it decides whether they press the button at
            all.
          */}
          <Card title="Permit2 allowances">
            <Permit2Allowances bot={bot.name} refreshKey={approvedAt} />
          </Card>
          <Card title="One-off runs">
            <OneShotRunner
              bot={bot.name}
              canApprove={bot.canApprove}
              approveBlockedReason={bot.approveBlockedReason}
              approveBlockedBy={bot.approveBlockedBy}
              highlightPermit2={showPermit2Banner}
              onApproved={() => {
                setShowPermit2Banner(false)
                setApprovedAt((n) => n + 1)
              }}
              onStopForApproval={(target) => void act('stop', target)}
            />
          </Card>
          {/*
            Last, and after the one-off runs: it's the recovery tool for a bad
            release, not something to reach for on the way past.
          */}
          <Card title="Roll back to an earlier version">
            <VersionRollback
              bot={bot}
              onRolledBack={(message) => {
                setNote(message)
                void load()
                void loadUpdates()
              }}
            />
          </Card>
        </>
      )}

      {tab === 'dashboard' && (
        <StitchDashboardEmbed
          key={name}
          botName={name}
          operatorAddress={bot.config?.operatorAddress}
        />
      )}
    </div>
  )
}

function Detail({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <dt className="text-xs uppercase tracking-wide text-faint">{label}</dt>
      <dd className="mt-0.5">{children}</dd>
    </div>
  )
}

/**
 * Short operator address. A hot wallet with a known explorer becomes a link
 * to that address page. MPC signers stay plain text.
 */
function OperatorAddress({ config }: { config: ConfigBody | null }) {
  if (!config?.operatorAddress) return '—'
  const text = shortAddress(config.operatorAddress)
  const explorerUrl =
    config.signer === 'hot-wallet' ? config.explorerUrl : null
  if (!explorerUrl) {
    return <span title={config.operatorAddress}>{text}</span>
  }
  return (
    <a
      href={explorerUrl}
      target="_blank"
      rel="noreferrer"
      title={config.operatorAddress}
      className="underline hover:text-ink"
    >
      {text}
    </a>
  )
}

/**
 * The layout fix.
 *
 * A bot whose compose file mounts only the two config files keeps its slot-nonce
 * ledger inside the container, so recreating it starts nonces over and the next
 * orders it signs collide with ones already on the book. Migrating moves the files
 * into a per-bot directory that stays on the host.
 */
function MigrationPrompt({
  bot,
  onMigrated,
}: {
  bot: Bot
  onMigrated: (bot: Bot) => void
}) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [result, setResult] = useState<MigrationResult | null>(null)
  // Set when the server refused because it couldn't read the ledger, which is the
  // only situation where the accept-the-loss button should exist at all.
  const [ledgerReadFailed, setLedgerReadFailed] = useState(false)

  if (result) {
    return (
      <Card title="Layout migrated">
        <div className="space-y-3">
          <Banner tone={result.ledgerLoss ? 'warning' : 'success'}>
            {result.message}
          </Banner>
          {result.ledgerLoss && <Banner tone="warning">{result.ledgerLoss}</Banner>}
          <p className="text-sm text-muted">
            Moved {result.movedFiles.join(', ') || 'nothing'}
            {result.ledgersRecovered.length > 0 &&
              `; recovered ${result.ledgersRecovered.join(', ')}`}
            . {result.started ? 'The bot is running again.' : 'The bot is stopped.'}
          </p>
        </div>
      </Card>
    )
  }

  if (bot.layout !== 'flat-files') return null

  return (
    <Card title="This bot's nonce ledger isn't on the host">
      <div className="space-y-3">
        <p className="text-sm text-muted">
          Its compose service mounts <code>stitch.toml</code> and the key as
          individual files, so the slot-nonce ledger lives inside the container.
          Recreating or upgrading the container throws that ledger away, and the bot
          restarts its nonces — orders still resting on the book then collide with
          the new ones.
        </p>
        <p className="text-sm text-muted">
          Migrating stops the bot, moves its config and key into a directory under
          the panel's bots root, copies the existing ledger out of the container, and
          starts it again on the same image. Expect a gap in quoting of a few
          seconds.
        </p>

        {!bot.canMigrate && bot.migrateBlockedReason && (
          <Banner tone="warning">{bot.migrateBlockedReason}</Banner>
        )}
        {error && <Banner tone="danger">{error}</Banner>}

        <Button
          variant="primary"
          busy={busy}
          disabled={!bot.canMigrate}
          onClick={async () => {
            if (
              !window.confirm(
                `Migrate ${bot.name} to the per-bot directory layout? It stops and restarts once.`,
              )
            ) {
              return
            }
            setBusy(true)
            setError(null)
            try {
              const res = await api.migrate(bot.name)
              setResult(res)
              onMigrated(res.bot)
            } catch (e) {
              setError(e instanceof ApiError ? e.message : String(e))
              // The server rolled back and left the bot as it was, so a retry is
              // free. Offer it inline rather than making the operator guess.
              setLedgerReadFailed(
                e instanceof ApiError && e.message.includes('accept ledger loss'),
              )
            } finally {
              setBusy(false)
            }
          }}
        >
          Migrate this bot
        </Button>

        {/*
          Only after a failed read, and only as a second, separate click: the first
          attempt already rolled back, so the ledger is still in the container and a
          retry is the right move for anything transient. This is for the case that
          never will read — a custom image with no run directory, or one too large to
          pull through the archive API.
        */}
        {ledgerReadFailed && (
          <Button
            busy={busy}
            onClick={async () => {
              if (
                !window.confirm(
                  `Migrate ${bot.name} without its nonce ledger? Any orders it has live right now stay on the book until they expire — it won't be able to replace them.`,
                )
              ) {
                return
              }
              setBusy(true)
              setError(null)
              try {
                const res = await api.migrate(bot.name, true)
                setResult(res)
                onMigrated(res.bot)
              } catch (e) {
                setError(e instanceof ApiError ? e.message : String(e))
              } finally {
                setBusy(false)
              }
            }}
          >
            Migrate without the ledger
          </Button>
        )}

        <p className="text-xs text-faint">
          Your compose file still describes the old layout. Update it from{' '}
          <ComposeExportLink className="underline">
            the generated compose export
          </ComposeExportLink>{' '}
          once you're done migrating, or compose will recreate the old mounts on the
          next <code>up</code>.
        </p>
      </div>
    </Card>
  )
}

import { useEffect, useRef, useState, type ReactNode } from 'react'
import { Link } from 'react-router-dom'
import { ApiError, api } from '../api'
import {
  Banner,
  Button,
  Card,
  ErrorState,
  Field,
  Input,
  Loading,
  Toggle,
} from './ui'
import { shortAddress } from '../format'
import type { Bot, Corridor, Settings, Spread } from '../types'

/**
 * The Corridors tab: every pool on this bot, its spreads and price feed, plus
 * a collapsed Experimental card for opt-in knobs (taker leg, TWAP,
 * inventory-lean). Quoting is always on; the venue flow owns the RFQ switch,
 * and the Textile overrides live under Tools. Sizing / tick stay on the Raw
 * config tab.
 *
 * Sends only the fields the operator touched — a partial patch means a concurrent
 * raw edit only loses what this form actually changed.
 */
export default function SettingsForm({
  bot,
  onSaved,
}: {
  bot: Bot
  onSaved: (message: string) => void
}) {
  const [loaded, setLoaded] = useState<Settings | null>(null)
  const [draft, setDraft] = useState<Settings | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  // A pool add, remove or switch is in flight in CorridorsCard. It renumbers
  // pools, so a Save that raced it would PATCH this pool index against whatever
  // corridor ends up at it — the removed pool's spreads landing on the pool
  // that survived.
  const [poolsChanging, setPoolsChanging] = useState(false)
  const [pool, setPool] = useState(0)

  // Reload when the bot's first-pool corridor changes (switch replaces stitch.toml)
  // or when the operator picks a different pool to edit.
  const corridorId = bot.config?.corridorId ?? ''
  // SettingsForm is reused across bots. A leftover pool 1+ on a one-pool bot
  // 400s the load and leaves only ErrorState — no control to get back.
  const poolKey = `${bot.name}:${corridorId}`
  const [poolOwner, setPoolOwner] = useState(poolKey)
  if (poolOwner !== poolKey) {
    setPoolOwner(poolKey)
    setPool(0)
  }
  const activePool = poolOwner === poolKey ? pool : 0
  // Which pool the form is on *now*, readable from an in-flight request's
  // continuation. `activePool` is captured per render, so a promise started
  // before a switch can't see it moved.
  const activePoolRef = useRef(activePool)
  activePoolRef.current = activePool
  useEffect(() => {
    let cancelled = false
    setLoaded(null)
    setLoadError(null)
    api
      .settings(bot.name, activePool)
      .then((s) => {
        if (cancelled) return
        setLoaded(s)
        setDraft(s)
      })
      .catch((e) => {
        if (!cancelled) setLoadError(e instanceof ApiError ? e.message : String(e))
      })
    return () => {
      cancelled = true
    }
  }, [bot.name, corridorId, activePool])

  if (loadError) return <ErrorState error={loadError} />
  if (!loaded || !draft) return <Loading what="the settings" />

  const dirty = JSON.stringify(loaded) !== JSON.stringify(draft)
  // Functional update so two sets in one handler (e.g. clearing TWAP window +
  // deviation together) both land — a spread from a stale `draft` would drop the first.
  const set = <K extends keyof Settings>(key: K, value: Settings[K]) =>
    setDraft((prev) => (prev ? { ...prev, [key]: value } : prev))

  async function save() {
    const saving = activePool
    setBusy(true)
    setError(null)
    try {
      const res = await api.saveSettings(
        bot.name,
        changedFields(loaded!, draft!),
      )
      // A save carries a restart, so the pool picker can move before it
      // answers. Writing this response then would put another corridor's
      // settings in the form while the list still highlights the one we left,
      // and the reload for that pool has already been and gone.
      if (saving !== activePoolRef.current) {
        onSaved(res.message)
        return
      }
      setLoaded(res.settings)
      setDraft(res.settings)
      onSaved(res.message)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  function applyPoolResult(res: { settings: Settings; message: string }) {
    setPool(res.settings.poolIndex)
    setLoaded(res.settings)
    setDraft(res.settings)
    onSaved(res.message)
  }

  return (
    <div className="space-y-4">
      <CorridorsCard
        bot={bot}
        settings={loaded}
        dirty={dirty}
        saving={busy}
        onBusyChange={setPoolsChanging}
        onSelectPool={(index) => {
          if (busy) return
          if (index === activePool) return
          if (
            dirty &&
            !window.confirm('Discard unsaved settings for this corridor?')
          ) {
            return
          }
          setPool(index)
        }}
        onPoolsChanged={applyPoolResult}
      />

      {/* No wallet change here: a bot is named after its wallet, and the
          wizard makes a new bot for a new wallet. Moving a bot between keys
          would leave the name, the venue registration and the approvals
          pointing at the old one. */}
      {/* Only while there is something to do here: not connected, waiting on
          Textile, or the old public-ladder mode to switch off. A seated bot
          says so in the state pill; Reconnect lives under Tools. */}
      {loaded.rfqPanelUnlocked &&
        !(loaded.rfqEnabled && loaded.rfqApiKeySet && loaded.rfqMakerId.trim() !== '' && !loaded.bookEnabled) && (
        <RfqCard
          botName={bot.name}
          loaded={loaded}
          pendingPatch={changedFields(loaded, draft)}
          editable={loaded.editable}
          onConnected={(next, message) => {
            setPool(next.poolIndex)
            setLoaded(next)
            setDraft(next)
            onSaved(message)
          }}
        />
      )}

      <VaultCard bot={bot} address={loaded.vaultAddress} />

      <Card
        title="Spreads"
        action={
          <span className="text-xs text-faint">
            {loaded.pools.find((p) => p.index === loaded.poolIndex)?.pair ??
              `${shortAddress(loaded.pair.collateral)} / ${shortAddress(loaded.pair.debt)}`}
          </span>
        }
      >
        <div className="space-y-4">
          <div className="grid gap-4 sm:grid-cols-2">
            <SpreadField
              label="Buy spread"
              hint="How far below the mid the bot bids."
              value={draft.buy}
              disabled={!loaded.editable}
              onChange={(v) => set('buy', v)}
            />
            <SpreadField
              label="Sell spread"
              hint="How far above the mid the bot asks."
              value={draft.sell}
              disabled={!loaded.editable}
              onChange={(v) => set('sell', v)}
            />
          </div>
          {loaded.bookEnabled && (
          <div className="grid gap-4 border-t border-line-soft pt-4 sm:grid-cols-2">
            <Field
              label="Order lifetime (seconds)"
              hint="How long each resting order stays live. Must be greater than 30 — shorter orders never show as fillable depth. Volatile pairs often use ~60. Book only — RFQ uses the venue TTL."
            >
              <Input
                type="number"
                min={31}
                step={1}
                value={draft.ttlSecs}
                disabled={!loaded.editable}
                onChange={(e) => {
                  const n = e.target.valueAsNumber
                  if (Number.isFinite(n) && n >= 0) set('ttlSecs', Math.trunc(n))
                }}
              />
            </Field>
            <Field
              label="Refresh threshold (bps)"
              hint="Re-quote a side when its price moves more than this. 0 re-posts every tick (usual with TWAP). A small deadband cuts signing churn on slow feeds."
            >
              <Input
                type="number"
                min={0}
                step={1}
                value={draft.refreshThresholdBps}
                disabled={!loaded.editable}
                onChange={(e) => {
                  const n = e.target.valueAsNumber
                  if (Number.isFinite(n) && n >= 0) {
                    set('refreshThresholdBps', Math.trunc(n))
                  }
                }}
              />
            </Field>
          </div>
          )}
        </div>
      </Card>

      <Card title="Endpoints">
        <div className="space-y-4">
          <Field label="RPC URL" hint="Where the bot reads chain state and sends transactions.">
            <Input
              value={draft.rpcUrl}
              onChange={(e) => set('rpcUrl', e.target.value)}
              disabled={!loaded.editable}
            />
          </Field>
          <Field label="Price feed URL">
            <Input
              value={draft.feedUrl}
              onChange={(e) => set('feedUrl', e.target.value)}
              disabled={!loaded.editable}
            />
          </Field>
        </div>
      </Card>

      {/* TWAP and lean are not book-only: the taker leg prices its fills off
          the same center, deviation guard and lean decision, so these stay
          editable on an RFQ-only bot. */}
      <ExperimentalCard
        draft={draft}
        editable={loaded.editable}
        onChange={set}
      />


      {error && <Banner tone="danger">{error}</Banner>}

      <div className="sticky bottom-4 flex items-center gap-3 rounded-xl border border-line-soft bg-surface p-3">
        <Button
          variant="primary"
          busy={busy}
          disabled={!dirty || !loaded.editable || poolsChanging}
          onClick={() => void save()}
        >
          {bot.running ? 'Save and restart' : 'Save'}
        </Button>
        <Button
          disabled={!dirty}
          onClick={() => setDraft(loaded)}
        >
          Discard
        </Button>
        <p className="text-xs text-faint">
          {!dirty
            ? 'No unsaved changes.'
            : bot.running
              ? loaded.bookEnabled
                ? 'Saving restarts the bot: it reads its config once at startup. Orders already signed stay on the book until they expire.'
                : 'Saving restarts the bot: it reads its config once at startup. In-flight RFQ quotes stay valid until they expire.'
              : 'The bot is stopped, so this only writes the file. It picks the change up when you start it.'}
        </p>
      </div>
    </div>
  )
}

/**
 * Every [[pools]] entry on this bot, plus add and remove.
 *
 * Add appends a same-chain catalog corridor so one process quotes two pairs.
 */
function CorridorsCard({
  bot,
  settings,
  dirty,
  saving,
  onBusyChange,
  onSelectPool,
  onPoolsChanged,
}: {
  bot: Bot
  settings: Settings
  dirty: boolean
  // A settings save is in flight, so the form is pinned to its pool.
  saving: boolean
  // Report add/remove/switch progress up, so the parent can lock Save while
  // the pool list is being renumbered underneath it.
  onBusyChange: (busy: boolean) => void
  onSelectPool: (index: number) => void
  onPoolsChanged: (res: { settings: Settings; message: string }) => void
}) {
  const [corridors, setCorridors] = useState<Corridor[] | null>(null)
  const [busy, setBusy] = useState<'remove' | null>(null)
  const [error, setError] = useState<string | null>(null)
  useEffect(() => {
    onBusyChange(busy !== null)
  }, [busy, onBusyChange])

  useEffect(() => {
    let cancelled = false
    api
      .corridors()
      .then((r) => {
        if (!cancelled) setCorridors(r.corridors)
      })
      .catch(() => {
        if (!cancelled) setCorridors([])
      })
    return () => {
      cancelled = true
    }
  }, [])

  const chainId = bot.config?.chainId
  const already = new Set(
    settings.pools.map((p) => p.corridorId).filter((id): id is string => !!id),
  )
  const live = (corridors ?? []).filter((c) => !c.pendingDeploy)
  const addable = live.filter(
    (c) => chainId != null && c.chainId === chainId && !already.has(c.id),
  )

  const selected =
    settings.pools.find((p) => p.index === settings.poolIndex) ?? settings.pools[0]

  function discardUnsavedOk() {
    return (
      !dirty || window.confirm('Discard unsaved settings for this corridor?')
    )
  }

  async function remove() {
    if (settings.poolCount < 2 || !selected) return
    if (!discardUnsavedOk()) return
    if (
      !window.confirm(
        `Remove ${selected.pair} from ${bot.name}?\n\nSpreads for that pair are dropped. A running bot restarts.`,
      )
    ) {
      return
    }
    setBusy('remove')
    setError(null)
    try {
      const res = await api.removePool(
        bot.name,
        selected.index,
        selected.collateral,
        selected.debt,
      )
      onPoolsChanged(res)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(null)
    }
  }

  return (
    <Card title="Corridors">
      <p className="text-sm text-ink">
        {settings.poolCount === 1
          ? 'Corridors in a single bot share the same capital.'
          : `This bot quotes ${settings.poolCount} pairs from one wallet. Pick one to edit its spreads, sizing and feed.`}
      </p>
      <ul className="mt-3 space-y-2">
        {settings.pools.map((p) => {
          const selectedPool = p.index === settings.poolIndex
          return (
            <li key={`${p.index}-${p.corridorId ?? p.pair}`}>
              <button
                type="button"
                onClick={() => onSelectPool(p.index)}
                disabled={saving && !selectedPool}
                className={`flex w-full items-center justify-between rounded-lg border px-3 py-2 text-left text-sm disabled:cursor-not-allowed disabled:opacity-50 ${
                  selectedPool
                    ? 'border-accent bg-accent/10 font-bold'
                    : 'border-line-soft hover:bg-hover'
                }`}
              >
                <span>{p.pair}</span>
                {selectedPool && (
                  <span className="text-xs font-normal text-muted">editing</span>
                )}
              </button>
            </li>
          )
        })}
      </ul>

      {/* One add path, and it is the wizard's. This card used to call addPool
          on its own and then tell the operator, in a confirm dialog, to go and
          enroll the bot's maker key by hand: a pool with an empty
          `rfq_corridor` answers nothing while the fleet page shows the bot
          healthy. The wizard does the enrolment itself, so this hands over to
          it rather than offering a second, quieter way to get it wrong. */}
      {settings.editable && addable.length > 0 && (
        <div className="mt-3">
          <Link
            to={`/add?bot=${encodeURIComponent(bot.name)}&chain=${chainId}`}
            className="inline-block"
          >
            <Button disabled={saving}>Add another corridor…</Button>
          </Link>
        </div>
      )}

      {settings.editable && settings.poolCount > 1 && (
        <div className="mt-3">
          <Button
            variant="danger"
            busy={busy === 'remove'}
            disabled={!selected || saving}
            onClick={() => void remove()}
          >
            Remove {selected?.pair ?? 'this corridor'}
          </Button>
        </div>
      )}

      {error && <Banner tone="danger">{error}</Banner>}
    </Card>
  )
}

// Exported so the add-bot wizard's Spread step is literally this field.
export function SpreadField({
  label,
  hint,
  value,
  disabled,
  onChange,
}: {
  label: string
  hint: string
  value: Spread
  disabled: boolean
  onChange: (v: Spread) => void
}) {
  const unit = value.kind === 'bps' ? 'bps' : 'abs'
  return (
    <Field label={`${label} (${unit})`} hint={hint}>
      <Input
        value={value.value}
        disabled={disabled}
        // "3" is the wizard's Normal quick pick, so the empty field and the
        // chips suggest the same scale.
        placeholder={value.kind === 'bps' ? '3' : '0.0015'}
        onChange={(e) => onChange({ ...value, value: e.target.value })}
      />
    </Field>
  )
}

/**
 * Opt-in knobs that aren't part of the everyday settings surface. Closed by
 * default so the main form stays short; each feature group is its own
 * subsection so later experiments can drop in beside TWAP / lean.
 */
/**
 * Where the bot's capital sits: its own wallet, or an OperatorVault Benoit
 * attaches to the bot's config. Read-only here on purpose: the vault is set
 * up outside the panel for now, and a switch that only half of the setup
 * honours would be a lie. Bot-wide, like the wallet it stands in for.
 */
function VaultCard({ bot, address }: { bot: Bot; address: string }) {
  const active = address.trim() !== ''
  const explorer = bot.config?.vaultExplorerUrl ?? null
  return (
    <Card title="Vault">
      <div className="flex flex-wrap items-center gap-3 text-sm">
        <span
          className={`inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-bold ${
            active ? 'bg-success-bg text-success' : 'bg-hover text-muted'
          }`}
        >
          {active ? 'Active' : 'Not active'}
        </span>
        {active ? (
          <>
            <span className="font-mono" title={address}>
              {shortAddress(address)}
            </span>
            {explorer && (
              <a className="text-xs text-accent underline" href={explorer} target="_blank" rel="noreferrer">
                View on explorer
              </a>
            )}
          </>
        ) : (
          <span className="text-muted">This bot trades from its own wallet.</span>
        )}
      </div>
    </Card>
  )
}

function ExperimentalCard({
  draft,
  editable,
  onChange,
}: {
  draft: Settings
  editable: boolean
  onChange: <K extends keyof Settings>(key: K, value: Settings[K]) => void
}) {
  const [open, setOpen] = useState(false)
  const leanOn = draft.leanEnabled || draft.leanShadow
  return (
    <Card>
      <button
        type="button"
        className={`-m-1 flex w-full items-center gap-2 rounded-lg p-1 text-left hover:bg-hover ${open ? 'mb-4' : ''}`}
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        <span
          aria-hidden
          className={`inline-block text-xs text-muted transition-transform ${open ? 'rotate-90' : ''}`}
        >
          ▸
        </span>
        <h2 className="text-base font-bold">Experimental</h2>
        <span className="rounded bg-hover px-1.5 py-0.5 text-[10px] font-bold uppercase tracking-wide text-muted">
          optional
        </span>
      </button>
      {open && (
        <div className="space-y-6">
          <p className="text-xs text-faint">
            Features here are opt-in and may change. Leave them alone unless you
            know you want them.
          </p>

          <ExperimentalSubsection
            title="Taker leg"
            description="On for every bot the panel creates. The bot fills users' resting limit orders when their price crosses your quote; fills are priced off the buy/sell spreads above, so a side with no spread is never taken. Each fill is an on-chain transaction from the wallet, which is why Approve and Withdraw need the bot stopped."
          >
            <Toggle
              checked={draft.takerEnabled}
              disabled={!editable}
              onChange={(v) => onChange('takerEnabled', v)}
              label="Take resting orders that cross this bot's quote"
            />
          </ExperimentalSubsection>

          <ExperimentalSubsection
            title="TWAP / lean"
            description="Center quotes on a rolling TWAP and/or lean spreads against the wallet's own inventory. These price the public ladder and the taker leg — Swap quotes (RFQ) answer off the latest feed print and your spreads. Useful on volatile pairs; leave blank unless you want them."
          >
            <div className="space-y-4">
              <div className="grid gap-4 sm:grid-cols-2">
                <Field
                  label="TWAP window (seconds)"
                  hint="Rolling average of the feed. Empty = quote the instantaneous mid."
                >
                  <Input
                    value={draft.twapWindowSecs}
                    placeholder="e.g. 60"
                    disabled={!editable}
                    onChange={(e) => {
                      const next = e.target.value
                      onChange('twapWindowSecs', next)
                      // Deviation only applies with a window. Clearing the window while
                      // leaving a populated deviation fails the loader; clear both so
                      // "turn TWAP off" is one field and one save.
                      if (
                        next.trim() === '' &&
                        draft.twapMaxDeviationBps.trim() !== ''
                      ) {
                        onChange('twapMaxDeviationBps', '')
                      }
                    }}
                  />
                </Field>
                <Field
                  label="TWAP max deviation (bps)"
                  hint="Never post a side more than this through spot. Empty = 50. Only applies with a TWAP window."
                >
                  <Input
                    value={draft.twapMaxDeviationBps}
                    placeholder="50"
                    disabled={!editable || draft.twapWindowSecs.trim() === ''}
                    onChange={(e) =>
                      onChange('twapMaxDeviationBps', e.target.value)
                    }
                  />
                </Field>
              </div>

              <div className="space-y-3 border-t border-line-soft pt-4">
                <Toggle
                  checked={draft.leanShadow}
                  disabled={!editable}
                  onChange={(v) => onChange('leanShadow', v)}
                  label="Lean shadow — log lean quotes next to the live ones (no behavior change)"
                />
                <Toggle
                  checked={draft.leanEnabled}
                  disabled={!editable}
                  onChange={(v) => onChange('leanEnabled', v)}
                  label="Lean enabled — price live quotes and taker fills off inventory-lean prices"
                />
                {leanOn && (
                  <Banner tone="warning">
                    Lean needs a measured <code>lean_floor_bps</code> (p95 feed
                    error vs live Pyth). Measure it first; don&apos;t assume a
                    number.
                  </Banner>
                )}
                <div className="grid gap-4 sm:grid-cols-3">
                  <Field
                    label="Lean floor (bps)"
                    hint="Required when lean is on. Measured p95 feed error."
                  >
                    <Input
                      value={draft.leanFloorBps}
                      placeholder="e.g. 3.0"
                      disabled={!editable}
                      onChange={(e) => onChange('leanFloorBps', e.target.value)}
                    />
                  </Field>
                  <Field
                    label="Lean base (bps)"
                    hint="Balanced-zone half-spread. Empty = 1.0."
                  >
                    <Input
                      value={draft.leanBaseBps}
                      placeholder="1.0"
                      disabled={!editable}
                      onChange={(e) => onChange('leanBaseBps', e.target.value)}
                    />
                  </Field>
                  <Field
                    label="Lean wide (bps)"
                    hint="Extra widening at the heavy edge. Empty = 3.0."
                  >
                    <Input
                      value={draft.leanWideBps}
                      placeholder="3.0"
                      disabled={!editable}
                      onChange={(e) => onChange('leanWideBps', e.target.value)}
                    />
                  </Field>
                </div>
              </div>
            </div>
          </ExperimentalSubsection>
        </div>
      )}
    </Card>
  )
}

/** One feature group inside the Experimental card. Add siblings for new experiments. */
function ExperimentalSubsection({
  title,
  description,
  children,
}: {
  title: string
  description: string
  children: ReactNode
}) {
  return (
    <section className="space-y-3 border-t border-line-soft pt-4 first:border-t-0 first:pt-0">
      <header className="space-y-1">
        <h3 className="text-sm font-bold">{title}</h3>
        <p className="text-xs text-faint">{description}</p>
      </header>
      {children}
    </section>
  )
}

function RfqCard({
  botName,
  loaded,
  pendingPatch,
  editable,
  onConnected,
}: {
  botName: string
  loaded: Settings
  pendingPatch: Record<string, unknown>
  editable: boolean
  onConnected: (settings: Settings, message: string) => void
}) {
  const [connecting, setConnecting] = useState(false)
  const [migrating, setMigrating] = useState(false)
  const [connectError, setConnectError] = useState<string | null>(null)
  const [enrollment, setEnrollment] = useState<{
    makerSlug: string
    environment: string
    corridors: string[]
    flagged?: boolean
  } | null>(null)
  const [contactEmail, setContactEmail] = useState('')
  const [sending, setSending] = useState(false)
  const [checking, setChecking] = useState(false)
  const [emailVerified, setEmailVerified] = useState<boolean | null>(null)
  const [venueMessage, setVenueMessage] = useState<string | null>(null)

  const ga = loaded.rfqDefaultUnlocked
  const connected = loaded.rfqApiKeySet && loaded.rfqMakerId.trim() !== ''
  const onBook = loaded.bookEnabled

  async function connect() {
    setConnecting(true)
    setConnectError(null)
    try {
      const res = await api.enrollRfq(botName)
      setEnrollment(res.enrollment ?? null)
      onConnected(res.settings, res.message)
    } catch (e) {
      setConnectError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setConnecting(false)
    }
  }

  // Connect writes rfq_enabled=false when the venue returned no corridor
  // (flagged, or no RFQ pair on this chain). Token match is enough once live.
  const live = connected && loaded.rfqEnabled
  const waiting = connected && !live
  const makerFlagged = enrollment?.flagged === true

  async function sendVerifyEmail() {
    setSending(true)
    setConnectError(null)
    setVenueMessage(null)
    try {
      const res = await api.verifyRfqEmail(botName, {
        contactEmail: contactEmail.trim(),
      })
      setEmailVerified(res.emailVerified)
      setVenueMessage(res.message)
    } catch (e) {
      setConnectError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setSending(false)
    }
  }

  async function checkStatus() {
    setChecking(true)
    setConnectError(null)
    setVenueMessage(null)
    try {
      const res = await api.checkRfqStatus(botName)
      setEmailVerified(res.emailVerified)
      setVenueMessage(res.message)
      if (res.contactEmail && !contactEmail.trim()) {
        setContactEmail(res.contactEmail)
      }
      if (res.enrollment) setEnrollment(res.enrollment)
      if (res.settings) onConnected(res.settings, res.message)
    } catch (e) {
      setConnectError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setChecking(false)
    }
  }

  async function switchToRfqOnly() {
    setMigrating(true)
    setConnectError(null)
    try {
      // Same patch the Save button would send, plus the book off. Replacing
      // draft from the server response would otherwise drop unsaved edits.
      const res = await api.saveSettings(botName, {
        ...pendingPatch,
        bookEnabled: false,
      })
      onConnected(res.settings, res.message)
    } catch (e) {
      setConnectError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setMigrating(false)
    }
  }

  return (
    <Card title="Textile connection">
      <div className="space-y-4">
        {ga && onBook && (
          <Banner tone="warning">
            This bot still posts a public ladder nobody sees on Swap. Switch to
            RFQ only — it will stop resting unused book orders and quote Swap
            with its full inventory.
            {live ? (
              <span className="mt-3 block">
                <Button
                  variant="primary"
                  busy={migrating}
                  disabled={!editable}
                  onClick={() => void switchToRfqOnly()}
                >
                  Switch to RFQ only
                </Button>
              </span>
            ) : (
              <span className="mt-2 block text-xs">
                {waiting
                  ? makerFlagged
                    ? 'Textile has blocked this maker. You will not receive Swap quotes until they unblock you.'
                    : 'Confirm your email address below to finish the switch.'
                  : 'Connect below to finish the switch.'}
              </span>
            )}
          </Banner>
        )}

        {/* No on/off switch: answering quote requests is what a bot is for.
            `rfq_enabled` still exists in the config, but the venue flow owns
            it (Connect and the email confirmation turn it on; a flagged maker
            turns it off), never a click here. Stop the bot to stop quoting. */}
        {waiting ? (
          <Banner tone="warning">
            Registered
            {enrollment
              ? ` as ${enrollment.makerSlug} (${enrollment.environment})`
              : ''}
            .{' '}
            {makerFlagged
              ? 'Textile has blocked this maker. You will not receive Swap quotes.'
              : emailVerified
                ? 'Your address is confirmed. Press Check status to pick the seats up.'
                : 'Confirm your email address below. That is the only step left — no Swap quotes until you do.'}
          </Banner>
        ) : live ? null : (
          <Banner tone="warning">
            {ga
              ? 'Not connected. This bot will not quote until you connect to Textile.'
              : 'Not connected. Connect to start answering Swap quote requests.'}
          </Banner>
        )}

        {connectError && <Banner tone="danger">{connectError}</Banner>}
        {venueMessage && !connectError && (
          <Banner tone={emailVerified ? 'success' : 'info'}>
            {venueMessage}
          </Banner>
        )}

        {waiting && !makerFlagged && (
          <div className="space-y-3 rounded-lg border border-line-soft p-3">
            <p className="text-sm font-bold">Confirm your email</p>
            <p className="text-xs text-faint">
              Use an address you own. Clicking the link we send seats this bot
              on every Swap pair, on every chain, including pairs Textile lists
              later. Sending again to the same address resends the link.
            </p>
            <Field label="Email">
              <Input
                type="email"
                value={contactEmail}
                disabled={!editable || sending}
                placeholder="you@desk.com"
                autoComplete="email"
                onChange={(e) => setContactEmail(e.target.value)}
              />
            </Field>
            <div className="flex flex-wrap gap-2">
              <Button
                variant="primary"
                busy={sending}
                disabled={!editable || !contactEmail.trim()}
                onClick={() => void sendVerifyEmail()}
              >
                Send the confirmation link
              </Button>
              <Button
                variant="secondary"
                busy={checking}
                disabled={!editable}
                onClick={() => void checkStatus()}
              >
                Check status
              </Button>
            </div>
          </div>
        )}

        {!connected && (
          <Button
            variant="primary"
            busy={connecting}
            disabled={!editable}
            onClick={() => void connect()}
          >
            {ga && onBook ? 'Connect and switch to RFQ' : 'Connect to Textile'}
          </Button>
        )}

      </div>
    </Card>
  )
}

/**
 * Only the fields that changed, plus the pool index the API needs. Shared with
 * the Textile overrides under Tools, which edit the `rfq*` keys the same way.
 */
export function changedFields(
  loaded: Settings,
  draft: Settings,
  rfqApiKey = '',
): Record<string, unknown> {
  // The pair goes with the index: the panel refuses a multi-corridor write
  // whose index has been renumbered by someone else's add or remove.
  const editing = loaded.pools.find((p) => p.index === loaded.poolIndex)
  const patch: Record<string, unknown> = {
    pool: loaded.poolIndex,
    collateral: editing?.collateral ?? loaded.pair.collateral,
    debt: editing?.debt ?? loaded.pair.debt,
  }
  const keys: (keyof Settings)[] = [
    'rpcUrl',
    'feedUrl',
    'buy',
    'sell',
    'takerEnabled',
    'ttlSecs',
    'refreshThresholdBps',
    'twapWindowSecs',
    'twapMaxDeviationBps',
    'leanEnabled',
    'leanShadow',
    'leanFloorBps',
    'leanBaseBps',
    'leanWideBps',
    'rfqEnabled',
    'rfqUrl',
    'rfqMakerId',
    'rfqValidationContract',
    'rfqCorridor',
    'bookEnabled',
    'vaultAddress',
  ]
  for (const key of keys) {
    if (JSON.stringify(loaded[key]) !== JSON.stringify(draft[key])) {
      patch[key] = draft[key]
    }
  }
  const key = rfqApiKey.trim()
  if (key) patch.rfqApiKey = key
  return patch
}

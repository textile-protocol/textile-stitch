import { useEffect, useRef, useState, type ReactNode } from 'react'
import { ApiError, api } from '../api'
import {
  Banner,
  Button,
  Card,
  ErrorState,
  Field,
  Input,
  Loading,
  Select,
  Toggle,
} from './ui'
import ChangeSigner from './ChangeSigner'
import { shortAddress } from '../format'
import type { Bot, Corridor, Settings, Sizing, Spread } from '../types'

/**
 * Structured settings matching the desktop Stitch app: corridor, signer, spreads,
 * taker leg, endpoints, plus a collapsed Experimental card for opt-in knobs
 * (TWAP / inventory-lean). The RFQ card is always on — new bots quote Swap
 * via RFQ. Only the genuinely book-only fields (order lifetime, refresh
 * threshold) hide when the ladder is off, and the collapsed Legacy card is
 * where the ladder itself can be put back. Sizing / tick stay on the Raw
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
  const [rfqApiKey, setRfqApiKey] = useState('')
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

  const dirty =
    JSON.stringify(loaded) !== JSON.stringify(draft) || rfqApiKey.trim() !== ''
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
        changedFields(loaded!, draft!, rfqApiKey),
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
      setRfqApiKey('')
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
    setRfqApiKey('')
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
        onSwitched={(message) => {
          setPool(0)
          onSaved(message)
        }}
      />

      <ChangeSigner
        bot={bot.name}
        chainId={bot.config?.chainId}
        wantsToBeUp={bot.state === 'running' || bot.state === 'restarting'}
        onChanged={onSaved}
      />

      {loaded.rfqPanelUnlocked && (
        <RfqCard
          botName={bot.name}
          draft={draft}
          loaded={loaded}
          rfqApiKey={rfqApiKey}
          pendingPatch={changedFields(loaded, draft, rfqApiKey)}
          corridorId={
            loaded.pools.find((p) => p.index === loaded.poolIndex)
              ?.corridorId ?? ''
          }
          editable={loaded.editable}
          onChange={set}
          onApiKey={setRfqApiKey}
          onConnected={(next, message) => {
            setPool(next.poolIndex)
            setLoaded(next)
            setDraft(next)
            setRfqApiKey('')
            onSaved(message)
          }}
        />
      )}

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

      <Card title="Taker leg">
        <Toggle
          checked={draft.takerEnabled}
          disabled={!loaded.editable}
          onChange={(v) => set('takerEnabled', v)}
          label="Take resting orders that cross this bot's quote"
        />
        <p className="mt-2 text-xs text-faint">
          Fill users' resting limit orders when their price crosses your quote.
          Fills are priced off the buy/sell spreads above, so a side with no
          spread is never taken.
        </p>
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

      {loaded.rfqDefaultUnlocked && (
        <LegacyCard
          loaded={loaded}
          draft={draft}
          editable={loaded.editable}
          onChange={set}
        />
      )}

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
          onClick={() => {
            setDraft(loaded)
            setRfqApiKey('')
          }}
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
 * Every [[pools]] entry on this bot, plus add / remove / replace.
 *
 * Add appends a same-chain catalog corridor so one process quotes two pairs.
 * Switch still replaces the whole file — keep it as the escape hatch.
 */
function CorridorsCard({
  bot,
  settings,
  dirty,
  saving,
  onBusyChange,
  onSelectPool,
  onPoolsChanged,
  onSwitched,
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
  onSwitched: (message: string) => void
}) {
  const [corridors, setCorridors] = useState<Corridor[] | null>(null)
  const [adding, setAdding] = useState(false)
  const [addChoice, setAddChoice] = useState('')
  const [switching, setSwitching] = useState(false)
  const [switchChoice, setSwitchChoice] = useState('')
  const [busy, setBusy] = useState<'add' | 'remove' | 'switch' | null>(null)
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
  const switchable = live

  const selected =
    settings.pools.find((p) => p.index === settings.poolIndex) ?? settings.pools[0]

  function discardUnsavedOk() {
    return (
      !dirty || window.confirm('Discard unsaved settings for this corridor?')
    )
  }

  async function add() {
    if (!discardUnsavedOk()) return
    if (
      !window.confirm(
        `Add this corridor to ${bot.name}?\n\nThe bot stays on this chain and keeps its signer. A running bot restarts. Approve Permit2 on the new tokens. RFQ only answers corridors this maker key is enrolled on.`,
      )
    ) {
      return
    }
    setBusy('add')
    setError(null)
    try {
      const res = await api.addPool(bot.name, addChoice)
      setAdding(false)
      onPoolsChanged(res)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(null)
    }
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

  async function applySwitch() {
    if (
      !window.confirm(
        `Replace ${bot.name}'s whole config with a different corridor?\n\nThis drops every pool and writes the preset (your signer is kept). A running bot is stopped — approve Permit2 for the new corridor's tokens before starting.`,
      )
    ) {
      return
    }
    setBusy('switch')
    setError(null)
    try {
      const res = await api.switchCorridor(bot.name, switchChoice)
      setSwitching(false)
      onSwitched(res.message)
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
          ? 'This bot quotes one pair. Add another corridor on the same chain to quote both from one wallet.'
          : `This bot quotes ${settings.poolCount} pairs on this chain. The fields below edit the selected one.`}
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

      {settings.editable && addable.length > 0 && !adding && (
        <div className="mt-3">
          <Button
            onClick={() => {
              setAddChoice(addable[0]!.id)
              setAdding(true)
              setSwitching(false)
              setError(null)
            }}
          >
            Add corridor…
          </Button>
        </div>
      )}
      {adding && (
        <div className="mt-3 space-y-3">
          <Field
            label="Add"
            hint="Appends a [[pools]] block. Same chain, same wallet, own price feed."
          >
            <Select
              value={addChoice}
              onChange={(e) => setAddChoice(e.target.value)}
            >
              {addable.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.displayName} — {c.networkLabel}
                </option>
              ))}
            </Select>
          </Field>
          <div className="flex gap-2">
            <Button
              variant="primary"
              busy={busy === 'add'}
              disabled={!addChoice}
              onClick={() => void add()}
            >
              Add corridor
            </Button>
            <Button
              onClick={() => {
                setAdding(false)
                setError(null)
              }}
            >
              Cancel
            </Button>
          </div>
        </div>
      )}

      {settings.editable && settings.poolCount > 1 && (
        <div className="mt-3">
          <Button
            variant="danger"
            busy={busy === 'remove'}
            disabled={!selected}
            onClick={() => void remove()}
          >
            Remove {selected?.pair ?? 'this corridor'}
          </Button>
        </div>
      )}

      {corridors && switchable.length >= 2 && !switching && (
        <div className="mt-4 border-t border-line-soft pt-3">
          <Button
            variant="ghost"
            onClick={() => {
              setSwitchChoice(bot.config?.corridorId ?? switchable[0]!.id)
              setSwitching(true)
              setAdding(false)
              setError(null)
            }}
          >
            Replace entire config…
          </Button>
        </div>
      )}
      {switching && corridors && (
        <div className="mt-3 space-y-3">
          <Field
            label="Replace with"
            hint="Drops every pool and writes the corridor preset. Spreads reset; the signer stays."
          >
            <Select
              value={switchChoice}
              onChange={(e) => setSwitchChoice(e.target.value)}
            >
              {switchable.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.displayName} — {c.networkLabel}
                </option>
              ))}
            </Select>
          </Field>
          <div className="flex gap-2">
            <Button
              variant="primary"
              busy={busy === 'switch'}
              disabled={!switchChoice}
              onClick={() => void applySwitch()}
            >
              Replace config
            </Button>
            <Button
              onClick={() => {
                setSwitching(false)
                setError(null)
              }}
            >
              Cancel
            </Button>
          </div>
        </div>
      )}
      {error && <Banner tone="danger">{error}</Banner>}
    </Card>
  )
}

function SpreadField({
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
        placeholder={value.kind === 'bps' ? '25' : '0.0015'}
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

/**
 * Can this side actually post? Mirrors `PoolConfig::buy_enabled` / `sell_enabled`
 * on the bot: a spread *and* a size — either a flat order size or a
 * total-liquidity + min-slice ladder. Without one the ladder rests nothing, so
 * the card says so instead of offering a switch that does nothing.
 */
function sideCanPost(spread: Spread, sizing: Sizing): boolean {
  const has = (v: string) => v.trim() !== ''
  return (
    has(spread.value) &&
    (has(sizing.orderSize) ||
      (has(sizing.totalLiquidity) && has(sizing.minSliceDebt)))
  )
}

/**
 * Legacy: the public ladder.
 *
 * Collapsed by default and last on the page — Swap is RFQ now, and a bot that
 * rests orders on the book is quoting into a venue no taker sees. It stays
 * reachable because leftover book bots exist and an operator may need to put
 * one back while debugging.
 *
 * The toggle reflects the config, so a migrated or new (RFQ-only) bot opens
 * with it off. Turning it on writes `book_enabled = true` through the normal
 * Save, which restarts the bot; the API refuses the flip when no side has a
 * spread and a size, since that would restart into quoting nothing.
 */
function LegacyCard({
  loaded,
  draft,
  editable,
  onChange,
}: {
  loaded: Settings
  draft: Settings
  editable: boolean
  onChange: <K extends keyof Settings>(key: K, value: Settings[K]) => void
}) {
  const [open, setOpen] = useState(false)
  const canPost =
    sideCanPost(draft.buy, draft.buySizing) ||
    sideCanPost(draft.sell, draft.sellSizing)
  const turningOn = draft.bookEnabled && !loaded.bookEnabled
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
        <h2 className="text-base font-bold">Legacy</h2>
        <span className="rounded bg-hover px-1.5 py-0.5 text-[10px] font-bold uppercase tracking-wide text-muted">
          {loaded.bookEnabled ? 'ladder on' : 'off'}
        </span>
      </button>
      {open && (
        <div className="space-y-4">
          <p className="text-xs text-faint">
            Before RFQ, Stitch quoted by resting a ladder of signed orders on
            the public book. Swap no longer reads that book — it asks makers for
            a firm quote — so a ladder posted today is invisible to takers while
            still holding your inventory behind live orders. Leave this off
            unless you know why you want it.
          </p>

          <Toggle
            checked={draft.bookEnabled}
            disabled={!editable}
            onChange={(v) => onChange('bookEnabled', v)}
            label="Post a public ladder (book_enabled)"
          />

          <p className="text-xs text-faint">
            Sizing lives on the Raw config tab. The ladder also uses the order
            lifetime and refresh threshold under Spreads, which appear once it
            is on.
          </p>

          {turningOn && !canPost && (
            <Banner tone="danger">
              No side can post: a ladder needs a spread and a size. Set the
              sizing on the Raw config tab first — saving this now will be
              refused.
            </Banner>
          )}

          {turningOn && canPost && (
            <Banner tone="warning">
              Saving restarts the bot with the ladder on. It will sign and rest
              orders against your funded balance, and those orders stay live
              until they expire even if you turn this back off.
            </Banner>
          )}
        </div>
      )}
    </Card>
  )
}

const DEFAULT_RFQ_URL = 'wss://api.textilecredit.com/v2/maker/stream'

/**
 * RFQ. Happy path is Connect: the bot signs MakerEnroll and the panel writes
 * the credential. Paste fields stay under Advanced for manual overrides.
 * The key is write-only.
 */
function RfqCard({
  botName,
  draft,
  loaded,
  rfqApiKey,
  pendingPatch,
  corridorId,
  editable,
  onChange,
  onApiKey,
  onConnected,
}: {
  botName: string
  draft: Settings
  loaded: Settings
  rfqApiKey: string
  pendingPatch: Record<string, unknown>
  corridorId: string
  editable: boolean
  onChange: <K extends keyof Settings>(key: K, value: Settings[K]) => void
  onApiKey: (value: string) => void
  onConnected: (settings: Settings, message: string) => void
}) {
  const [connecting, setConnecting] = useState(false)
  const [migrating, setMigrating] = useState(false)
  const [connectError, setConnectError] = useState<string | null>(null)
  const [advanced, setAdvanced] = useState(false)
  const [enrollment, setEnrollment] = useState<{
    makerSlug: string
    environment: string
    corridors: string[]
    flagged?: boolean
  } | null>(null)
  const [contactEmail, setContactEmail] = useState('')
  const [contactWhatsapp, setContactWhatsapp] = useState('')
  const [requesting, setRequesting] = useState(false)
  const [checking, setChecking] = useState(false)
  const [accessStatus, setAccessStatus] = useState<
    'NONE' | 'PENDING' | 'APPROVED' | 'REJECTED' | null
  >(null)
  const [accessMessage, setAccessMessage] = useState<string | null>(null)

  const ga = loaded.rfqDefaultUnlocked
  const connected = loaded.rfqApiKeySet && loaded.rfqMakerId.trim() !== ''
  const onBook = loaded.bookEnabled

  function enable(next: boolean) {
    onChange('rfqEnabled', next)
    if (!next) return
    if (!draft.rfqUrl.trim()) onChange('rfqUrl', DEFAULT_RFQ_URL)
    const registered =
      loaded.rfqApiKeySet && loaded.rfqMakerId.trim() !== ''
    const waiting = registered && !draft.rfqCorridor.trim()
    // A registered bot with no venue corridor is flagged or has no RFQ pair.
    // Do not invent a slug from the book corridor — that is not an assignment.
    if (waiting) return
    if (!draft.rfqCorridor.trim() && corridorId) {
      onChange('rfqCorridor', corridorId)
    }
  }

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
  const rejected = accessStatus === 'REJECTED'

  async function requestAccess() {
    setRequesting(true)
    setConnectError(null)
    setAccessMessage(null)
    try {
      const res = await api.requestRfqAccess(botName, {
        contactEmail: contactEmail.trim() || undefined,
        contactWhatsapp: contactWhatsapp.trim() || undefined,
      })
      setAccessStatus(res.accessStatus)
      setAccessMessage(res.message)
      if (res.enrollment) setEnrollment(res.enrollment)
    } catch (e) {
      setConnectError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setRequesting(false)
    }
  }

  async function checkAccess() {
    setChecking(true)
    setConnectError(null)
    setAccessMessage(null)
    try {
      const res = await api.checkRfqAccess(botName)
      setAccessStatus(res.accessStatus)
      setAccessMessage(res.message)
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
    <Card title="RFQ">
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
                    ? 'This maker is flagged. You will not receive Swap quotes until Textile unflags you.'
                    : 'Request access below so Textile can review this maker.'
                  : 'Connect below to finish the switch.'}
              </span>
            )}
          </Banner>
        )}

        <Toggle
          checked={draft.rfqEnabled}
          disabled={!editable}
          onChange={enable}
          label="Answer Swap quote requests"
        />
        <p className="text-xs text-faint">
          Connect registers this bot&apos;s funding wallet and saves the
          credential. Textile still has to approve you before you receive
          Swap quotes — Request access below, then Check status after they
          do. You never paste an id or key. The venue rejects requests under 1
          whole token so the protocol fee cannot round to zero.
        </p>

        {waiting ? (
          <Banner tone="warning">
            Registered
            {enrollment
              ? ` as ${enrollment.makerSlug} (${enrollment.environment})`
              : ''}
            .{' '}
            {makerFlagged
              ? 'Textile has flagged this maker. You will not receive Swap quotes.'
              : rejected
                ? 'Textile turned this chain down. Request access again if you want another review.'
                : accessStatus === 'PENDING'
                  ? 'Access requested. Textile will review it. Check status after they approve you.'
                  : 'Request access so Textile can review this maker. You will not receive Swap quotes until they approve you.'}
          </Banner>
        ) : live ? (
          <div className="rounded-lg border border-line-soft bg-hover/40 px-3 py-2 text-sm">
            <p className="font-medium">
              Connected
              {enrollment
                ? ` as ${enrollment.makerSlug} (${enrollment.environment})`
                : ''}
              {ga && !onBook ? ' · Swap only' : ''}
            </p>
            <p className="mt-1 text-xs text-faint">
              {enrollment?.corridors.length
                ? `Corridors: ${enrollment.corridors.join(', ')}`
                : loaded.rfqCorridor
                  ? `Corridor: ${loaded.rfqCorridor}`
                  : 'No corridor assigned yet.'}
              {loaded.rfqApiKeySet ? ' · API key saved' : ''}
            </p>
          </div>
        ) : (
          <Banner tone="warning">
            {ga
              ? 'Not connected. This bot will not quote until you connect to Textile.'
              : 'Not connected. Connect to start answering Swap quote requests.'}
          </Banner>
        )}

        {connectError && <Banner tone="danger">{connectError}</Banner>}
        {accessMessage && !connectError && (
          <Banner tone={accessStatus === 'REJECTED' ? 'danger' : 'success'}>
            {accessMessage}
          </Banner>
        )}

        {waiting && !makerFlagged && (
          <div className="space-y-3 rounded-lg border border-line-soft p-3">
            <p className="text-sm font-bold">Request access</p>
            <p className="text-xs text-faint">
              Textile replies about the review by email, so that one is
              required. WhatsApp is optional — add it if you would rather they
              ping you there.
            </p>
            <Field label="Email">
              <Input
                type="email"
                value={contactEmail}
                disabled={!editable || requesting}
                placeholder="you@desk.com"
                autoComplete="email"
                onChange={(e) => setContactEmail(e.target.value)}
              />
            </Field>
            <Field label="WhatsApp (optional)">
              <Input
                type="tel"
                value={contactWhatsapp}
                disabled={!editable || requesting}
                placeholder="+15551234567"
                autoComplete="tel"
                onChange={(e) => setContactWhatsapp(e.target.value)}
              />
            </Field>
            <div className="flex flex-wrap gap-2">
              <Button
                variant="primary"
                busy={requesting}
                disabled={!editable || !contactEmail.trim()}
                onClick={() => void requestAccess()}
              >
                Request access
              </Button>
              <Button
                variant="secondary"
                busy={checking}
                disabled={!editable}
                onClick={() => void checkAccess()}
              >
                Check status
              </Button>
            </div>
          </div>
        )}

        <div className="space-y-1">
          <Button
            variant={connected ? 'ghost' : 'primary'}
            busy={connecting}
            disabled={!editable}
            onClick={() => void connect()}
          >
            {connected
              ? 'Reconnect to Textile'
              : ga && onBook
                ? 'Connect and switch to RFQ'
                : 'Connect to Textile'}
          </Button>
          {connected && (
            <p className="text-xs text-faint">
              Optional. Only if the session is stuck — not part of switching
              to RFQ.
            </p>
          )}
        </div>

        <div>
          <button
            type="button"
            className="text-xs text-muted underline hover:text-ink"
            onClick={() => setAdvanced((v) => !v)}
          >
            {advanced ? 'Hide advanced' : 'Advanced'}
          </button>
        </div>

        {advanced && (
          <div className="space-y-4 border-t border-line-soft pt-4">
            <p className="text-xs text-faint">
              Manual overrides. Use Connect above unless you were given these
              values to paste.
            </p>
            <Field
              label="Quote stream URL"
              hint="Where the bot listens for private quote requests. Production is wss://. ws:// is allowed only on localhost."
            >
              <Input
                value={draft.rfqUrl}
                disabled={!editable}
                placeholder={DEFAULT_RFQ_URL}
                onChange={(e) => onChange('rfqUrl', e.target.value)}
              />
            </Field>
            <Field
              label="Maker ID"
              hint="Textile's maker record ID (starts with cl or cm). Not the short display name."
            >
              <Input
                value={draft.rfqMakerId}
                disabled={!editable}
                placeholder="cl…"
                autoComplete="off"
                onChange={(e) => onChange('rfqMakerId', e.target.value)}
              />
            </Field>
            <Field
              label="Fill validation contract"
              hint="On-chain address that authorizes this maker to fill preferred quotes on this chain."
            >
              <Input
                value={draft.rfqValidationContract}
                disabled={!editable}
                placeholder="0x…"
                autoComplete="off"
                onChange={(e) => onChange('rfqValidationContract', e.target.value)}
              />
            </Field>
            <Field
              label="Corridor"
              hint="Trading corridor this bot quotes on (for example cngn-usdt-celo). Usually matches the bot's corridor."
            >
              <Input
                value={draft.rfqCorridor}
                disabled={!editable}
                placeholder={corridorId || 'cngn-usdt-bsc'}
                onChange={(e) => onChange('rfqCorridor', e.target.value)}
              />
            </Field>
            <Field
              label="API key"
              hint={
                loaded.rfqApiKeySet
                  ? 'A key is already saved. Paste a new one only to rotate it. The current value is never shown.'
                  : 'Starts with tx_live_…. Saved on disk for the panel owner only — never written to stitch.toml.'
              }
            >
              <Input
                type="password"
                value={rfqApiKey}
                disabled={!editable}
                placeholder={loaded.rfqApiKeySet ? '••••••••' : 'tx_live_…'}
                autoComplete="off"
                onChange={(e) => onApiKey(e.target.value)}
              />
            </Field>
          </div>
        )}
      </div>
    </Card>
  )
}

/** Only the fields this form edits, plus the pool index the API needs. */
function changedFields(
  loaded: Settings,
  draft: Settings,
  rfqApiKey: string,
): Record<string, unknown> {
  const patch: Record<string, unknown> = { pool: loaded.poolIndex }
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

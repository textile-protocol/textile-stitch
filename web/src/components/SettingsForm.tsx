import { useEffect, useState, type ReactNode } from 'react'
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
import type { Bot, Corridor, Settings, Spread } from '../types'

/**
 * Structured settings matching the desktop Stitch app: corridor, signer, spreads,
 * taker leg, endpoints, plus a collapsed Experimental card for opt-in knobs
 * (TWAP / inventory-lean today; more subsections can land beside it).
 * Sizing / tick stay on the Raw config tab.
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

  // Reload when the bot's corridor changes (switch replaces stitch.toml wholesale).
  const corridorId = bot.config?.corridorId ?? ''
  useEffect(() => {
    let cancelled = false
    setLoaded(null)
    setLoadError(null)
    // Desktop edits pool 0 only; multi-pool configs keep other pools via raw TOML.
    api
      .settings(bot.name, 0)
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
  }, [bot.name, corridorId])

  if (loadError) return <ErrorState error={loadError} />
  if (!loaded || !draft) return <Loading what="the settings" />

  const dirty = JSON.stringify(loaded) !== JSON.stringify(draft)
  // Functional update so two sets in one handler (e.g. clearing TWAP window +
  // deviation together) both land — a spread from a stale `draft` would drop the first.
  const set = <K extends keyof Settings>(key: K, value: Settings[K]) =>
    setDraft((prev) => (prev ? { ...prev, [key]: value } : prev))

  async function save() {
    setBusy(true)
    setError(null)
    try {
      const res = await api.saveSettings(bot.name, changedFields(loaded!, draft!))
      setLoaded(res.settings)
      setDraft(res.settings)
      onSaved(res.message)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-4">
      <CorridorCard bot={bot} onSwitched={onSaved} />

      <ChangeSigner
        bot={bot.name}
        chainId={bot.config?.chainId}
        wantsToBeUp={bot.state === 'running' || bot.state === 'restarting'}
        onChanged={onSaved}
      />

      {loaded.poolCount > 1 && (
        <Banner tone="warning">
          This config has {loaded.poolCount} pools. The fields below edit pool 1
          only — change the others in Raw config.
        </Banner>
      )}

      <Card
        title="Spreads"
        action={
          <span className="text-xs text-faint">
            {shortAddress(loaded.pair.collateral)} / {shortAddress(loaded.pair.debt)}
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
          <div className="grid gap-4 border-t border-line-soft pt-4 sm:grid-cols-2">
            <Field
              label="Order lifetime (seconds)"
              hint="How long each resting order stays live. Must be greater than 30 — shorter orders never show as fillable depth. Volatile pairs often use ~60."
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

      <ExperimentalCard
        draft={draft}
        editable={loaded.editable}
        onChange={set}
      />

      {loaded.rfqPanelUnlocked && <RfqBetaCard settings={loaded} />}

      {error && <Banner tone="danger">{error}</Banner>}

      <div className="sticky bottom-4 flex items-center gap-3 rounded-xl border border-line-soft bg-surface p-3">
        <Button
          variant="primary"
          busy={busy}
          disabled={!dirty || !loaded.editable}
          onClick={() => void save()}
        >
          {bot.running ? 'Save and restart' : 'Save'}
        </Button>
        <Button disabled={!dirty} onClick={() => setDraft(loaded)}>
          Discard
        </Button>
        <p className="text-xs text-faint">
          {!dirty
            ? 'No unsaved changes.'
            : bot.running
              ? 'Saving restarts the bot: it reads its config once at startup. Orders already signed stay on the book until they expire.'
              : 'The bot is stopped, so this only writes the file. It picks the change up when you start it.'}
        </p>
      </div>
    </div>
  )
}

/**
 * Current corridor with a deliberate "Switch corridor…" affordance. Switching
 * replaces stitch.toml with the preset (signer preserved) and stops a running bot.
 */
function CorridorCard({
  bot,
  onSwitched,
}: {
  bot: Bot
  onSwitched: (message: string) => void
}) {
  const [corridors, setCorridors] = useState<Corridor[] | null>(null)
  const [switching, setSwitching] = useState(false)
  const [choice, setChoice] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

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

  // Corridors a bot can actually be moved onto. Pending ones ship a preset
  // with a placeholder reactor, and the API refuses them anyway.
  const switchable = (corridors ?? []).filter((c) => !c.pendingDeploy)

  const current =
    bot.config?.corridorLabel ??
    (bot.config?.corridorId ? bot.config.corridorId : 'Custom corridor')

  async function apply() {
    if (
      !window.confirm(
        `Switch ${bot.name} to a different corridor?\n\nThis replaces stitch.toml with the preset (your signer is kept). A running bot is stopped — approve Permit2 for the new corridor's tokens (needs a little gas) before starting.`,
      )
    ) {
      return
    }
    setBusy(true)
    setError(null)
    try {
      const res = await api.switchCorridor(bot.name, choice)
      setSwitching(false)
      onSwitched(res.message)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Card title="Corridor">
      <p className="text-sm text-ink">{current}</p>
      {corridors && switchable.length >= 2 && !switching && (
        <div className="mt-3">
          <Button
            onClick={() => {
              setChoice(bot.config?.corridorId ?? switchable[0]!.id)
              setSwitching(true)
              setError(null)
            }}
          >
            Switch corridor…
          </Button>
        </div>
      )}
      {switching && corridors && (
        <div className="mt-3 space-y-3">
          <Field
            label="Switch to"
            hint="Replaces this bot's config with the corridor preset. Spreads and endpoints reset; the signer stays."
          >
            <Select value={choice} onChange={(e) => setChoice(e.target.value)}>
              {switchable.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.displayName} — {c.networkLabel}
                </option>
              ))}
            </Select>
          </Field>
          {error && <Banner tone="danger">{error}</Banner>}
          <div className="flex gap-2">
            <Button
              variant="primary"
              busy={busy}
              disabled={!choice || choice === bot.config?.corridorId}
              onClick={() => void apply()}
            >
              Switch corridor
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
            description="Center quotes on a rolling TWAP and/or lean spreads against the wallet's own inventory. Useful on volatile pairs; leave blank unless you want them."
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
                  label="Lean enabled — quote the live book off inventory-lean prices"
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
 * Read-only status for the RFQ responder beta. Rendered ONLY when the config's
 * [experimental] gate unlocks it — with the gate off this component never
 * mounts and the page is pixel-identical to a build without it. Deliberately
 * no controls: the [rfq] block is edited by hand (Raw config tab), never here.
 */
function RfqBetaCard({ settings }: { settings: Settings }) {
  return (
    <Card title="RFQ (beta)">
      <div className="space-y-3">
        <p className="text-xs text-faint">
          Dual-run pilot: the bot answers the venue&apos;s private quote
          requests beside the public ladder. Configured in stitch.toml under
          [rfq]; this card is informational only.
        </p>
        <dl className="grid gap-x-4 gap-y-2 text-sm sm:grid-cols-[auto_1fr]">
          <dt className="text-faint">State</dt>
          <dd>{settings.rfqEnabled ? 'Enabled' : 'Disabled'}</dd>
          <dt className="text-faint">Corridors</dt>
          <dd>
            {settings.rfqCorridors.length > 0
              ? settings.rfqCorridors.join(', ')
              : '—'}
          </dd>
          <dt className="text-faint">Stream</dt>
          <dd className="break-all">{settings.rfqUrl || '—'}</dd>
        </dl>
      </div>
    </Card>
  )
}

/** Only the fields this form edits, plus the pool index the API needs. */
function changedFields(loaded: Settings, draft: Settings): unknown {
  const patch: Record<string, unknown> = { pool: 0 }
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
  ]
  for (const key of keys) {
    if (JSON.stringify(loaded[key]) !== JSON.stringify(draft[key])) {
      patch[key] = draft[key]
    }
  }
  return patch
}

import { useEffect, useState } from 'react'
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
import { formatAtomic, formatSeconds, shortAddress } from '../format'
import type { Bot, Settings, Sizing, Spread } from '../types'

/**
 * The structured settings form.
 *
 * Sends only the fields the operator touched, because the API takes a partial
 * patch — that way a form that doesn't show a field can't clear it. Amounts are
 * edited as the atomic-unit integers the config stores, with the human-readable
 * value shown underneath rather than substituted for it: rounding an inventory
 * into a float and back is how you post an order for the wrong size.
 */
export default function SettingsForm({
  bot,
  onSaved,
}: {
  bot: Bot
  onSaved: (message: string) => void
}) {
  const [pool, setPool] = useState(0)
  const [loaded, setLoaded] = useState<Settings | null>(null)
  const [draft, setDraft] = useState<Settings | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    let cancelled = false
    setLoaded(null)
    setLoadError(null)
    api
      .settings(bot.name, pool)
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
  }, [bot.name, pool])

  if (loadError) return <ErrorState error={loadError} />
  if (!loaded || !draft) return <Loading what="the settings" />

  const dirty = JSON.stringify(loaded) !== JSON.stringify(draft)
  const set = <K extends keyof Settings>(key: K, value: Settings[K]) =>
    setDraft({ ...draft, [key]: value })

  async function save() {
    setBusy(true)
    setError(null)
    try {
      const res = await api.saveSettings(bot.name, changedFields(loaded!, draft!, pool))
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
      {loaded.poolCount > 1 && (
        <Card title="Pool">
          <Field
            label="Editing pool"
            hint={`This config has ${loaded.poolCount} pools. The fields below apply to the one selected here; tick cadence is bot-wide.`}
          >
            <Select value={pool} onChange={(e) => setPool(Number(e.target.value))}>
              {Array.from({ length: loaded.poolCount }, (_, i) => (
                <option key={i} value={i}>
                  Pool {i + 1}
                </option>
              ))}
            </Select>
          </Field>
        </Card>
      )}

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

      <Card
        title="Spreads"
        action={
          <span className="text-xs text-faint">
            {shortAddress(loaded.pair.collateral)} / {shortAddress(loaded.pair.debt)}
          </span>
        }
      >
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
        <div className="mt-4">
          <Toggle
            checked={draft.takerEnabled}
            disabled={!loaded.editable}
            onChange={(v) => set('takerEnabled', v)}
            label="Take resting orders that cross this bot's quote"
          />
        </div>
      </Card>

      <Card title="Sizing">
        <div className="grid gap-6 sm:grid-cols-2">
          <SizingFields
            title="Buy side"
            unit={`sized in debt · ${loaded.pair.debtDecimals} decimals`}
            decimals={loaded.pair.debtDecimals}
            debtDecimals={loaded.pair.debtDecimals}
            value={draft.buySizing}
            disabled={!loaded.editable}
            onChange={(v) => set('buySizing', v)}
          />
          <SizingFields
            title="Sell side"
            unit={`sized in collateral · ${loaded.pair.collateralDecimals} decimals`}
            decimals={loaded.pair.collateralDecimals}
            debtDecimals={loaded.pair.debtDecimals}
            value={draft.sellSizing}
            disabled={!loaded.editable}
            onChange={(v) => set('sellSizing', v)}
          />
        </div>
        <p className="mt-4 text-xs text-faint">
          With a total and a minimum slice set, the bot quotes a ladder and ignores
          the flat order size. Leave the ladder pair empty to quote one order per
          side instead. <code>max</code> in a total means "everything funded".
        </p>
      </Card>

      <Card title="Timing">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field
            label="Order lifetime (seconds)"
            hint={`${formatSeconds(draft.ttlSecs)} — orders this bot signs stay fillable this long, even after it stops.`}
          >
            <Input
              type="number"
              min={1}
              value={draft.ttlSecs}
              disabled={!loaded.editable}
              onChange={(e) => set('ttlSecs', Number(e.target.value))}
            />
          </Field>
          <Field
            label="Tick interval (seconds)"
            hint={`${formatSeconds(draft.tickIntervalSecs)} between re-quotes. Applies to the whole bot, not just this pool.`}
          >
            <Input
              type="number"
              min={1}
              value={draft.tickIntervalSecs}
              disabled={!loaded.editable}
              onChange={(e) => set('tickIntervalSecs', Number(e.target.value))}
            />
          </Field>
        </div>
      </Card>

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
  return (
    <Field label={label} hint={hint}>
      <div className="flex gap-2">
        <Input
          value={value.value}
          disabled={disabled}
          placeholder={value.kind === 'bps' ? '25' : '0.0015'}
          onChange={(e) => onChange({ ...value, value: e.target.value })}
        />
        <Select
          value={value.kind}
          disabled={disabled}
          className="w-28"
          onChange={(e) => onChange({ ...value, kind: e.target.value as Spread['kind'] })}
        >
          <option value="bps">bps</option>
          <option value="abs">absolute</option>
        </Select>
      </div>
    </Field>
  )
}

function SizingFields({
  title,
  unit,
  decimals,
  debtDecimals,
  value,
  disabled,
  onChange,
}: {
  title: string
  unit: string
  /** Decimals of this side's own token: debt when buying, collateral when selling. */
  decimals: number
  /** Debt decimals, which is what the minimum slice is always denominated in. */
  debtDecimals: number
  value: Sizing
  disabled: boolean
  onChange: (v: Sizing) => void
}) {
  // `maxOrders` is a count, so it has no scale and gets no converted hint.
  const rows: [keyof Sizing, string, number | null][] = [
    ['totalLiquidity', 'Total ladder liquidity', decimals],
    ['minSliceDebt', 'Smallest slice (debt units)', debtDecimals],
    ['orderSize', 'Flat order size', decimals],
    ['maxOrders', 'Maximum live orders', null],
  ]
  return (
    <div className="space-y-3">
      <h3 className="text-sm font-bold">
        {title} <span className="font-normal text-faint">{unit}</span>
      </h3>
      {rows.map(([key, label, scale]) => (
        <Field
          key={key}
          label={label}
          hint={
            scale === null || value[key].trim() === ''
              ? undefined
              : `= ${formatAtomic(value[key], scale)}`
          }
        >
          <Input
            value={value[key]}
            disabled={disabled}
            inputMode="numeric"
            placeholder="unset"
            onChange={(e) => onChange({ ...value, [key]: e.target.value })}
          />
        </Field>
      ))}
    </div>
  )
}

/**
 * Only the fields that actually changed, plus the pool they apply to.
 *
 * Sending the whole view back would work, but a partial patch means a concurrent
 * edit through the raw editor only loses the fields this form actually touched.
 */
function changedFields(loaded: Settings, draft: Settings, pool: number): unknown {
  const patch: Record<string, unknown> = { pool }
  const keys: (keyof Settings)[] = [
    'rpcUrl',
    'feedUrl',
    'buy',
    'sell',
    'takerEnabled',
    'buySizing',
    'sellSizing',
    'ttlSecs',
    'tickIntervalSecs',
  ]
  for (const key of keys) {
    if (JSON.stringify(loaded[key]) !== JSON.stringify(draft[key])) {
      patch[key] = draft[key]
    }
  }
  return patch
}

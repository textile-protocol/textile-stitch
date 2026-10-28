import { useEffect, useRef, useState } from 'react'
import { api, type RecallFleetView } from '../api'
import type { Bot } from '../types'
import { Banner, Button, Input } from './ui'

export function PerpsFeature({ bot, dirty }: { bot: Bot; dirty: boolean }) {
  const initialized = useRef(false)
  const [id, setId] = useState('')
  const [enabled, setEnabled] = useState(false)
  const [recallEnabled, setRecallEnabled] = useState(false)
  const [recall, setRecall] = useState<RecallFleetView | null>(null)
  const [diagnosis, setDiagnosis] = useState('')
  const [fundingTarget, setFundingTarget] = useState('')
  const [savedFundingTarget, setSavedFundingTarget] = useState('')
  const [mode, setMode] = useState('off')
  const [effective, setEffective] = useState('shadow')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  useEffect(() => {
    let active = true
    setLoading(true)
    setError(null)
    initialized.current = false
    async function refresh() {
      try {
        const value = await api.perps(bot.name)
        if (!active) return
        if (!initialized.current) {
          setId(value.assignmentId)
          setRecallEnabled(value.recallEnabled ?? false)
          setFundingTarget(value.fundingTargetAtoms ?? '')
        }
        setSavedFundingTarget(value.fundingTargetAtoms ?? '')
        setRecall(value.remote?.recall ?? null)
        initialized.current = true
        setEnabled(value.enabled)
        setMode(
          value.enabled ? (value.remote?.control?.mode ?? 'unknown') : 'off'
        )
        setEffective(
          value.unavailable
            ? 'unavailable'
            : (value.remote?.effectiveMode ?? 'shadow')
        )
        setError(
          value.unavailable
            ? 'Service unavailable. Turning off still stops local authorization.'
            : null
        )
      } catch (e) {
        if (active) setError(String(e))
      } finally {
        if (active) setLoading(false)
      }
    }
    void refresh()
    const interval = window.setInterval(() => void refresh(), 5000)
    return () => {
      active = false
      window.clearInterval(interval)
    }
  }, [bot.name])
  async function save(next: 'off' | 'preview' | 'live') {
    if (
      next === 'live' &&
      !window.confirm(
        'Enable FX hedge execution for this Stitch using its assigned margin account and caps? This restarts the bot.'
      )
    )
      return
    setBusy(true)
    setError(null)
    try {
      await api.setPerps(bot.name, id, next, recallEnabled, fundingTarget)
      const value = await api.perps(bot.name)
      setSavedFundingTarget(value.fundingTargetAtoms ?? '')
      setEnabled(value.enabled)
      setMode(
        value.enabled ? (value.remote?.control?.mode ?? 'unknown') : 'off'
      )
      setEffective(value.remote?.effectiveMode ?? 'shadow')
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }
  return (
    <section className="border-line-soft space-y-3 border-t pt-5">
      <h3 className="font-bold">FX hedge</h3>
      <p className="text-muted text-sm">
        Hedge this Stitch’s FX inventory with its assigned margin account.
      </p>
      <p className="text-sm">
        Feature: {mode}. Authorization:{' '}
        {effective === 'shadow' ? 'preview only' : effective}.
      </p>
      <label className="block text-sm">
        Hedge assignment
        <Input
          value={id}
          onChange={(e) => setId(e.target.value)}
          disabled={
            busy || loading || enabled || mode !== 'off' || !bot.editable
          }
          placeholder="Assignment supplied by Textile ops"
        />
      </label>
      <label className="flex items-center gap-2 text-sm">
        <input
          type="checkbox"
          checked={recallEnabled}
          onChange={(e) => setRecallEnabled(e.target.checked)}
          disabled={
            busy ||
            loading ||
            !bot.editable ||
            (!recallEnabled && !recall?.liveLimitsConfigured)
          }
        />
        Allow firm quotes backed by GMX collateral
      </label>
      <p className="text-muted text-xs">
        The original price stays fixed. Accepted recalls prepare for settlement
        within 90 seconds of the request. A failed recall fails the trade; gas
        may still be spent.
      </p>
      <label className="block text-sm">
        Authorized margin allocation (token atoms;{' '}
        {10 ** (recall?.manifest.assets.collateral.decimals ?? 0)} atoms per
        token)
        <Input
          value={fundingTarget}
          onChange={(e) => setFundingTarget(e.target.value)}
          placeholder="0 — no automatic transfers"
          disabled={busy || !bot.editable || !recallEnabled}
        />
      </label>
      <p className="text-muted text-xs">
        Use additional allocated liquidity. Stitch transfers only inventory
        unreserved by quotes, up to this target and the deployment capital
        limits.
      </p>
      <div className="flex gap-2">
        {(['off', 'preview', 'live'] as const).map((next) => (
          <Button
            key={next}
            disabled={
              busy ||
              loading ||
              (dirty && next !== 'off') ||
              !bot.editable ||
              !id ||
              (mode === next &&
                !(next === 'off' && enabled) &&
                recallEnabled === recall?.enabled &&
                fundingTarget === savedFundingTarget)
            }
            onClick={() => void save(next)}
          >
            {next === 'off'
              ? 'Turn off'
              : next === 'preview'
                ? 'Preview'
                : 'Enable hedging'}
          </Button>
        ))}
      </div>
      <p className="text-muted text-xs">
        Off stops new hedges. Open positions can still be reduced, closed or
        liquidated. Preview checks the feed without trading. Only explicitly
        assigned Stitches can enable execution.
      </p>
      {recall && <RecallStatus recall={recall} />}
      {recall?.position?.pauseReason && (
        <div className="space-y-2">
          <Input
            value={diagnosis}
            onChange={(e) => setDiagnosis(e.target.value)}
            placeholder="Diagnosis before resuming recalls"
          />
          <Button
            disabled={busy || !bot.editable || diagnosis.trim().length < 10}
            onClick={() => {
              setBusy(true)
              void api
                .resumeRecall(bot.name, diagnosis.trim())
                .then(() => setDiagnosis(''))
                .catch((e) => setError(String(e)))
                .finally(() => setBusy(false))
            }}
          >
            Resume after diagnosis
          </Button>
        </div>
      )}
      {dirty && (
        <p className="text-muted text-xs">Save your other settings first.</p>
      )}
      {error && <Banner tone="danger">{error}</Banner>}
    </section>
  )
}

function RecallStatus({ recall }: { recall: RecallFleetView }) {
  const p = recall.position
  if (!p)
    return (
      <p className="text-muted text-sm">
        Waiting for a verified inventory snapshot.
      </p>
    )
  const units = (v: string) => {
    const scale = 10n ** BigInt(recall.manifest.assets.collateral.decimals)
    const n = BigInt(v)
    return `${n / scale}.${(n % scale).toString().padStart(recall.manifest.assets.collateral.decimals, '0')}`
  }
  return (
    <div className="space-y-2 text-sm">
      <dl className="grid grid-cols-2 gap-1">
        {Object.entries({
          Liquid: p.liquidAtoms,
          Collateral: p.collateralAtoms,
          Recallable: p.recallableAtoms,
          'Reserved collateral': recall.reservedCollateralAtoms,
          'Reserved liquid': recall.reservedLiquidAtoms,
          'Short token exposure': p.shortTokens,
          'Fee headroom': recall.manifest.limits?.feeHeadroomAtoms ?? '0',
        }).map(([name, amount]) => (
          <div key={name}>
            <dt className="text-muted">{name}</dt>
            <dd>{units(amount)}</dd>
          </div>
        ))}
      </dl>
      <p className="text-muted text-xs">
        Liquid balances and fee headroom may remain unhedged. Recallable
        capacity is conservative and changes with fees and PnL.
      </p>
      {p.pauseReason && (
        <Banner tone="danger">
          Recall paused: {p.pauseReason}. Existing obligations remain reserved.
        </Banner>
      )}
      {recall.operations.slice(0, 5).map((o) => (
        <p key={o.rfqId}>
          {o.state}: {o.failureReason ?? o.outcome ?? 'pending'} · preparation
          cost {o.preparationWei} wei
          {o.cancelAfter
            ? ` · cancellation eligible ${new Date(o.cancelAfter).toLocaleTimeString()}`
            : ''}
        </p>
      ))}
    </div>
  )
}

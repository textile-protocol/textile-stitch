import { useEffect, useState } from 'react'
import { type HedgeMode } from '../api'
import { useBalancesHidden, maskBalance } from '../balancePrivacy'
import { formatAtomic } from '../format'
import {
  allocationAtoms,
  allocationTokens,
  hedgeMode,
  hedgeSnapshotCurrent,
  hedgeStatus,
} from '../perps'
import type { Bot, Funding } from '../types'
import type { PerpsState } from '../usePerps'
import type { ModuleSection } from '../moduleWorkspace'
import { Disclosure } from './ModuleControls'
import ScrollTabs from './ScrollTabs'
import { Banner, Button, Card, Field, Input, Toggle } from './ui'

export default function PerpsModule({
  bot,
  perps,
  funding,
  section,
  onSection,
  onDirtyChange,
}: {
  bot: Bot
  perps: PerpsState
  funding: Funding | null
  section?: ModuleSection
  onDirtyChange?: (dirty: boolean) => void
  onSection?: (section: ModuleSection) => void
}) {
  const { view, loading, busy, error } = perps
  const [hidden, toggleHidden] = useBalancesHidden()
  const [assignment, setAssignment] = useState('')
  const [recallEnabled, setRecallEnabled] = useState(false)
  const [allocation, setAllocation] = useState('')
  const [diagnosis, setDiagnosis] = useState('')
  const recall = view?.remote?.recall
  const p = recall?.position
  const asset = recall?.manifest.assets.collateral
  const symbol =
    funding?.tokens.find(
      (t) => t.token.toLowerCase() === asset?.address.toLowerCase()
    )?.symbol ?? 'tokens'
  const mode = hedgeMode(view)
  const monitoring = view?.remote?.monitoring
  const current = Boolean(
    view &&
      !error &&
      !view.unavailable &&
      hedgeSnapshotCurrent(view, Date.now())
  )
  const unavailable = Boolean(error || view?.unavailable)
  const pending = monitoring
    ? monitoring.pendingActions + monitoring.pendingQuotes
    : null

  useEffect(() => {
    setAssignment(view?.assignmentId ?? '')
  }, [view?.assignmentId])
  useEffect(() => {
    setRecallEnabled(view?.recallEnabled ?? false)
  }, [view?.recallEnabled])
  useEffect(() => {
    setAllocation(
      asset
        ? allocationTokens(view?.fundingTargetAtoms ?? '0', asset.decimals)
        : ''
    )
  }, [view?.fundingTargetAtoms, asset?.decimals])

  const atoms = asset ? allocationAtoms(allocation, asset.decimals) : null
  const changes =
    view &&
    (assignment !== view.assignmentId ||
      recallEnabled !== view.recallEnabled ||
      (atoms !== null && atoms !== (view.fundingTargetAtoms || '0')))
  useEffect(() => {
    onDirtyChange?.(Boolean(changes))
  }, [changes, onDirtyChange])
  const liveBlocked =
    unavailable ||
    !current ||
    !recall?.liveLimitsConfigured ||
    recall.executionEnabled === false ||
    Boolean(p?.pauseReason) ||
    !recallEnabled
  const fmt = (value?: string, decimals = asset?.decimals) =>
    value === undefined || decimals === undefined
      ? '—'
      : maskBalance(formatAtomic(value, decimals), hidden)
  const total =
    p && p.marginIdleAtoms !== undefined
      ? BigInt(p.liquidAtoms) +
        BigInt(p.marginIdleAtoms) +
        BigInt(p.collateralAtoms)
      : null
  const net = total === null || !p ? null : total - BigInt(p.shortTokens)
  const netText =
    net === null
      ? '—'
      : maskBalance(
          `${net < 0n ? '−' : ''}${formatAtomic((net < 0n ? -net : net).toString(), asset!.decimals)}`,
          hidden
        )
  const title = hedgeStatus(view, Date.now(), Boolean(error))
  const at = monitoring?.observedAt ?? p?.observedAt

  async function save(next: HedgeMode) {
    if (
      next === 'live' &&
      !window.confirm(
        'Enable live FX hedging and collateral-backed quotes for this bot within its assigned limits? This restarts the bot.'
      )
    )
      return
    // Off uses the saved assignment and allocation, even if the editor has
    // invalid or unsaved changes. It must remain available during an outage.
    if (next === 'off') {
      await perps.save('off')
      return
    }
    await perps.save(next, {
      assignmentId: assignment.trim(),
      recallEnabled,
      fundingTargetAtoms: atoms ?? view?.fundingTargetAtoms ?? '',
    })
  }

  return (
    <Card
      className={
        section ? 'space-y-5 !rounded-none !border-0 !p-0' : 'space-y-5'
      }
    >
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div>
          <div className="flex items-center gap-3">
            <h2 className="text-xl font-bold">FX hedge</h2>
            <span
              className={`rounded-full px-2.5 py-1 text-xs font-bold ${title === 'Live' ? 'bg-success-bg text-success' : title === 'Paused' || unavailable ? 'bg-warning-bg text-warning' : 'bg-hover text-muted'}`}
            >
              {title}
            </span>
          </div>
          <p className="text-muted mt-2 text-sm">
            {mode === 'preview'
              ? 'Observe real inventory and positions without opening new hedges.'
              : mode === 'off'
                ? 'New hedge commitments are off. Existing positions and operations remain monitored.'
                : 'Manage this bot’s currency exposure and collateral-backed liquidity.'}
          </p>
        </div>
        <div className="flex gap-1" role="group" aria-label="FX hedge mode">
          {(['off', 'preview', 'live'] as const).map((next) => (
            <Button
              key={next}
              aria-pressed={mode === next}
              variant={mode === next ? 'primary' : 'secondary'}
              disabled={
                busy ||
                loading ||
                !view ||
                !bot.editable ||
                (next === 'off'
                  ? !view.assignmentId
                  : !assignment.trim() ||
                    unavailable ||
                    (asset !== undefined && atoms === null)) ||
                (next === 'live' && liveBlocked) ||
                (mode === next && !changes)
              }
              onClick={() => void save(next)}
            >
              {next === 'off' ? 'Off' : next === 'preview' ? 'Preview' : 'Live'}
            </Button>
          ))}
        </div>
      </div>

      {recall?.executionEnabled === false && (
        <p className="text-muted text-sm">
          Live execution is disabled in the deployment. Preview remains
          available.
        </p>
      )}
      {error && <Banner tone="danger">{error}</Banner>}
      {view?.unavailable && (
        <Banner tone="warning">
          Hedge service unavailable. Off still stops local authorization. Last
          known balances may be outdated.
        </Banner>
      )}
      {!loading && !recall && (
        <Banner tone="info">
          {view?.assignmentId
            ? 'Waiting for the keeper to verify the deployment and publish inventory. Missing values are not zero balances.'
            : section
              ? 'Choose the hedge assignment in Settings, then select Preview to enable this module.'
              : 'Choose the hedge assignment below, then select Preview to enable this module.'}
        </Banner>
      )}
      {p?.pauseReason && (
        <Banner tone="warning">
          Recall paused: {p.pauseReason}. Existing obligations remain reserved.
        </Banner>
      )}
      {p && !current && (
        <Banner tone="warning">
          Inventory snapshot is stale or unavailable. These are last known
          values, not current executable capacity.
        </Banner>
      )}

      {section && onSection && (
        <ScrollTabs
          label="Module sections"
          value={section}
          onChange={onSection}
          items={[
            { value: 'overview', label: 'Overview' },
            { value: 'settings', label: 'Settings' },
            { value: 'activity', label: 'Activity' },
          ]}
        />
      )}
      {changes && section && section !== 'settings' && (
        <p role="status" className="text-sm text-warning">
          Unsaved hedge settings. Your saved configuration is still in use.
        </p>
      )}
      <div
        hidden={section !== undefined && section !== 'overview'}
        className="border-line-soft border-t pt-4"
      >
        <div className="mb-4 flex items-center justify-between gap-3">
          <h3 className="text-sm font-bold">Inventory · {symbol}</h3>
          <Button variant="ghost" onClick={toggleHidden}>
            {hidden ? 'Show amounts' : 'Hide amounts'}
          </Button>
        </div>
        <dl className="grid grid-cols-2 gap-x-4 gap-y-5 lg:grid-cols-4">
          {Object.entries({
            'Liquid in bot': fmt(p?.liquidAtoms),
            'Idle in margin account': fmt(p?.marginIdleAtoms),
            'Deposited collateral': fmt(p?.collateralAtoms),
            'Recall capacity': current ? fmt(p?.recallableAtoms) : '—',
            'Reserved liquid': fmt(recall?.reservedLiquidAtoms),
            'Reserved collateral': fmt(recall?.reservedCollateralAtoms),
            'Current short exposure': fmt(p?.shortTokens),
            'Net unhedged inventory': netText,
          }).map(([label, value]) => (
            <div key={label}>
              <dt className="text-muted text-xs">{label}</dt>
              <dd className="mt-1 text-xl font-bold tabular-nums">{value}</dd>
            </div>
          ))}
        </dl>
        <p className="text-muted mt-4 text-xs">
          Recall capacity is an estimate before reservations. Reserved amounts
          are parts of the balances above, not additional funds. Target: cover
          inventory within available margin and the short exposure limit
          {recall?.manifest.limits?.shortUsd30
            ? ` ($${fmt(recall.manifest.limits.shortUsd30, 30)}).`
            : '.'}{' '}
          Fee headroom: {fmt(recall?.manifest.limits?.feeHeadroomAtoms)}{' '}
          {symbol}. Net exposure subtracts the short from held tokens; it does
          not include PnL or fees.
        </p>
        <p className="text-muted mt-2 text-xs">
          Last inventory update:{' '}
          {at ? new Date(at).toLocaleString() : 'Waiting for a snapshot'}.
        </p>
      </div>

      <div
        hidden={section !== undefined && section !== 'activity'}
        className="border-line-soft border-t pt-4"
      >
        <div className="mb-3 flex items-center justify-between">
          <h3 className="text-sm font-bold">Recent operations</h3>
          <span className="text-muted text-xs">
            {pending === null
              ? 'Pending count unavailable'
              : `${pending} pending`}
          </span>
        </div>
        {!recall?.operations.length &&
          !monitoring?.inventoryOperations.length && (
            <p className="text-muted text-sm">
              {recall
                ? 'No recorded funding, hedge or recall operations yet.'
                : 'Waiting for operation history.'}
            </p>
          )}
        <div className="space-y-2 text-sm">
          {monitoring?.inventoryOperations.map((o) => (
            <div key={o.id} className="bg-canvas rounded-lg p-3">
              <p className="font-bold capitalize">
                {o.kind} · {o.state}
              </p>
              <p className="text-muted text-xs">
                {new Date(o.createdAt).toLocaleString()}
                {o.failureReason ? ` · ${o.failureReason}` : ''}
              </p>
            </div>
          ))}
          {recall?.operations.slice(0, 5).map((o) => (
            <div key={o.rfqId} className="bg-canvas rounded-lg p-3">
              <p className="font-bold">Recall · {o.state}</p>
              <p className="text-muted text-xs">
                {o.failureReason ?? o.outcome ?? 'Pending'} · recorded
                preparation spend {fmt(o.preparationWei, 18)} CELO
              </p>
              {o.cancelAfter && (
                <p className="text-muted text-xs">
                  Cancellation eligible{' '}
                  {new Date(o.cancelAfter).toLocaleString()}
                </p>
              )}
            </div>
          ))}
        </div>
      </div>

      <div hidden={section !== undefined && section !== 'settings'}>
        <Disclosure
          title="Hedge configuration"
          open={section === 'settings' ? true : undefined}
        >
          <div className="space-y-4">
            <Field label="Hedge assignment">
              <Input
                value={assignment}
                onChange={(e) => setAssignment(e.target.value)}
                disabled={
                  busy ||
                  loading ||
                  !bot.editable ||
                  Boolean(view?.enabled) ||
                  Boolean(
                    view?.assignmentId &&
                      (!monitoring ||
                        monitoring.hasFunds ||
                        pending ||
                        !current)
                  )
                }
                placeholder="Assignment supplied by Textile ops"
              />
            </Field>
            <Toggle
              label="Allow firm quotes backed by GMX collateral"
              checked={recallEnabled}
              onChange={setRecallEnabled}
              disabled={
                busy ||
                !bot.editable ||
                (!recallEnabled && !recall?.liveLimitsConfigured)
              }
            />
            <Field label={`Authorized margin allocation (${symbol})`}>
              <Input
                value={hidden ? '' : allocation}
                onChange={(e) => setAllocation(e.target.value)}
                inputMode="decimal"
                disabled={
                  hidden || busy || !bot.editable || !asset || !recallEnabled
                }
                placeholder={
                  hidden ? 'Show amounts to edit' : '0 — no automatic transfers'
                }
              />
            </Field>
            {asset && atoms === null && (
              <p className="text-danger text-sm">
                Enter a nonnegative amount with at most {asset.decimals} decimal
                places.
              </p>
            )}
            <p className="text-muted text-xs">
              Only additional, unreserved inventory is transferred, within the
              deployment’s capital limits. Owner approvals are still required.
            </p>
            {!recallEnabled && (
              <p className="text-muted text-xs">
                Enable collateral-backed quotes here before selecting Live.
                Preview needs no trading permission.
              </p>
            )}
            <Button
              disabled={
                busy ||
                !bot.editable ||
                !changes ||
                mode === 'unknown' ||
                unavailable ||
                atoms === null ||
                (mode === 'live' && liveBlocked)
              }
              onClick={() => void save(mode === 'live' ? 'live' : 'preview')}
            >
              Save {mode === 'live' ? 'live settings' : 'in Preview'}
            </Button>
          </div>
        </Disclosure>
      </div>

      {p?.pauseReason && (
        <div className="flex flex-col gap-2 sm:flex-row">
          <Input
            aria-label="Diagnosis before resuming"
            value={diagnosis}
            onChange={(e) => setDiagnosis(e.target.value)}
            placeholder="Explain the diagnosis before resuming"
          />
          <Button
            disabled={
              busy ||
              !bot.editable ||
              unavailable ||
              diagnosis.trim().length < 10
            }
            onClick={() =>
              void perps.resume(diagnosis.trim()).then((saved) => {
                if (saved) setDiagnosis('')
              })
            }
          >
            Resume after diagnosis
          </Button>
        </div>
      )}
      <p className="text-muted text-xs">
        Off stops new collateral-backed quotes, margin funding and hedge
        increases. Ordinary trading, pending transactions and protective exits
        continue. {busy ? 'Saving and restarting the bot…' : ''}
      </p>
    </Card>
  )
}

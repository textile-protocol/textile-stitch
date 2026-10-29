import { curveOverviewState, freshReading } from '../curve'
import { type ModulesView } from '../modules'
import {
  moduleOverviewState,
  percentage,
  runningConfigDiffers,
  type ModuleKey,
} from '../modulePresentation'
import { CurveNow } from './CurveModule'
import { AllocationBar, Disclosure } from './ModuleControls'
import DynamicSpreadsLive from './DynamicSpreadsLive'
import LiquidityScheduleLive from './LiquidityScheduleLive'
import { Button, Card } from './ui'

export default function ModulesOverview({
  selected,
  view,
  now,
  currency,
  settlement,
  onDecisions,
}: {
  selected: ModuleKey
  view: ModulesView
  now: number
  currency: string
  settlement: string
  onDecisions: () => void
}) {
  // Older panels only served vault bots.
  if (selected === 'curve')
    return (
      <CurveOverview
        view={view}
        now={now}
        currency={currency}
        settlement={settlement}
        onDecisions={onDecisions}
      />
    )
  const state = moduleOverviewState(view, now)
  const config = view.status?.config ?? view.config
  const latest = view.status?.decisions.at(-1)?.decision
  const current = state.current ? latest : undefined
  const share = current?.inventory_bps
  const margins = current && !current.blocked
  const shadow = config.mode === 'shadow'
  const differs = runningConfigDiffers(view)
  if (selected === 'spreads')
    return <DynamicSpreadsLive view={view} now={now} />
  if (selected === 'liquidity')
    return (
      <LiquidityScheduleLive view={view} now={now} settlement={settlement} />
    )
  return (
    <div className="space-y-4">
      <Card className="!rounded-none !border-0 !p-0">
        <div className="flex flex-wrap items-start justify-between gap-4">
          <div>
            <p className="text-muted mb-2 text-xs font-bold uppercase tracking-wider">
              Right now
            </p>
            <h3 className="text-xl font-bold">{state.title}</h3>
            <p className="text-muted mt-2 max-w-xl text-sm">{state.message}</p>
          </div>
          <span className="bg-hover rounded-full px-3 py-1 text-xs font-bold">
            {view.running
              ? shadow
                ? 'Preview mode'
                : config.mode === 'off'
                  ? 'Off'
                  : 'Live mode'
              : 'Bot stopped'}
          </span>
        </div>
        {differs && (
          <p className="bg-warning-bg mt-4 rounded-lg p-3 text-sm text-warning">
            Saved settings differ from the bot’s last update. Check the restart
            status in details.
          </p>
        )}
        <div className="border-line-soft mt-6 grid gap-6 border-t pt-6 sm:grid-cols-2">
          <div>
            <p className="text-muted text-sm">
              {currency} · share of vault value
            </p>
            <p className="mb-4 mt-1 text-3xl font-bold tabular-nums">
              {share == null ? '—' : percentage(share)}
            </p>
            {share != null && config.inventory.enabled ? (
              <>
                <AllocationBar
                  share={share}
                  target={config.inventory.target_bps}
                  limit={config.inventory.max_bps}
                  currency={currency}
                />
                <p className="text-muted mt-3 text-sm">
                  {share >= config.inventory.max_bps
                    ? `${shadow ? 'Preview: purchases would pause' : 'Purchases paused'} above the limit.`
                    : share > config.inventory.target_bps
                      ? 'Above your inventory target.'
                      : share < config.inventory.target_bps
                        ? 'Below your inventory target.'
                        : 'At your inventory target.'}
                </p>
              </>
            ) : (
              <p className="text-muted text-sm">
                {share == null
                  ? 'Waiting for current vault data.'
                  : 'Share of vault value. Inventory balancing is off.'}
              </p>
            )}
          </div>
          <div className="sm:border-line-soft sm:border-l sm:pl-6">
            <p className="text-muted text-sm">
              {shadow ? 'Preview quote margins' : 'Current quote margins'}
            </p>
            <div className="my-4 grid grid-cols-2 gap-4">
              {(
                [
                  ['Vault buys', current?.buy_bps],
                  ['Vault sells', current?.sell_bps],
                ] as const
              ).map(([label, value]) => (
                <div key={label}>
                  <p className="text-muted text-xs">
                    {label} {currency}
                  </p>
                  <p className="mt-1 text-2xl font-bold tabular-nums">
                    {!margins || value === undefined
                      ? '—'
                      : value === null
                        ? 'Paused'
                        : percentage(value)}
                  </p>
                </div>
              ))}
            </div>
            <p className="text-muted text-sm">
              Margin from the reference price. A smaller margin makes that side
              more competitive.
            </p>
          </div>
        </div>
      </Card>
      {state.current && latest && !view.status?.decisions.at(-1)?.inputs && (
        <p className="bg-hover rounded-xl px-4 py-3 text-sm">
          Your bot reports basic readings. Update the bot on its card above to
          see price history and the spread breakdown. Updating only the panel
          does not add these readings.
        </p>
      )}
      <Disclosure title="Details & recent activity">
        <div className="space-y-4">
          <DynamicSpreadsLive view={view} now={now} />
          {latest && (
            <div className="text-sm">
              <p className="font-bold">
                Last recorded decision ·{' '}
                {new Date(latest.at * 1000).toLocaleString()}
              </p>
              <ul className="text-muted mt-2 space-y-1">
                {latest.reasons.map((reason, i) => (
                  <li key={i}>{reason}</li>
                ))}
              </ul>
            </div>
          )}
          <p className="text-muted text-xs">
            Holdings are valued in the settlement currency and include
            settlement funds deployed for yield. Quotes are calculations, not
            completed trades. Vault limits and available liquidity still apply.
          </p>
          <Button onClick={onDecisions}>Open decision history</Button>
        </div>
      </Disclosure>
    </div>
  )
}

/** Curve monitoring for both wallet- and vault-funded bots. */
function CurveOverview({
  view,
  now,
  currency,
  settlement,
  onDecisions,
}: {
  view: ModulesView
  now: number
  currency: string
  settlement: string
  onDecisions: () => void
}) {
  const vault = view.vault ?? true
  const state = curveOverviewState(view, now)
  const reading = freshReading(view.status?.curve, now)
  const quotes = view.status?.curve?.quotes ?? []
  const flat = quotes.filter((q) => q.flat_reason !== null).length
  return (
    <div className="space-y-4">
      <Card className="!rounded-none !border-0 !p-0">
        <div className="flex flex-wrap items-start justify-between gap-4">
          <div>
            <p className="text-muted mb-2 text-xs font-bold uppercase tracking-wider">
              Right now
            </p>
            <h3 className="text-xl font-bold">{state.title}</h3>
            <p className="text-muted mt-2 max-w-xl text-sm">
              {vault
                ? state.message.replace('this wallet', 'this vault')
                : state.message}
            </p>
          </div>
          <span className="bg-hover rounded-full px-3 py-1 text-xs font-bold">
            {state.pill}
          </span>
        </div>
        <div className="border-line-soft mt-6 border-t pt-6">
          <CurveNow
            view={{ ...view, vault }}
            now={now}
            currency={currency}
            settlement={settlement}
          />
        </div>
      </Card>
      <Disclosure title="Details & recent activity">
        <div className="space-y-4 text-sm">
          <dl className="grid gap-x-6 gap-y-3 sm:grid-cols-2">
            {(
              [
                [
                  'Reference book',
                  reading
                    ? `mid ${reading.reference_mid.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })} · half-range ${reading.half_range_bps.toFixed(1)} bps`
                    : '—',
                ],
                [
                  vault ? 'Vault value' : 'Wallet value',
                  reading
                    ? `${reading.value.toLocaleString(undefined, { maximumFractionDigits: 0 })} ${settlement}`
                    : '—',
                ],
                [
                  'Venue fee, last request',
                  reading ? `${reading.fee_bps} bps` : '—',
                ],
                [
                  'Answered flat',
                  quotes.length ? `${flat} of the last ${quotes.length}` : '—',
                ],
              ] as const
            ).map(([label, value]) => (
              <div key={label}>
                <dt className="text-muted text-xs">{label}</dt>
                <dd className="font-bold tabular-nums">{value}</dd>
              </div>
            ))}
          </dl>
          <p className="text-muted text-xs">
            {vault
              ? 'Holdings use the vault’s free balances and existing NAV valuation.'
              : 'Holdings use this wallet’s raw balances valued at the reference mid.'}{' '}
            This side’s open quotes count as already moved. Quotes are
            calculations, not completed trades.
          </p>
          <Button onClick={onDecisions}>Open quote history</Button>
        </div>
      </Disclosure>
    </div>
  )
}

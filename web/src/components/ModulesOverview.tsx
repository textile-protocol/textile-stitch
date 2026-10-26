import { curveOverviewState, freshReading } from '../curve'
import { type ModulesView } from '../modules'
import {
  moduleNames,
  moduleOverviewState,
  modulesFor,
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
  view,
  now,
  currency,
  settlement,
  onConfigure,
  onDecisions,
}: {
  view: ModulesView
  now: number
  currency: string
  settlement: string
  onConfigure: (key: ModuleKey) => void
  onDecisions: () => void
}) {
  // Older panels only served vault bots.
  if (!(view.vault ?? true))
    return (
      <WalletOverview
        view={view}
        now={now}
        currency={currency}
        settlement={settlement}
        onConfigure={onConfigure}
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
  const descriptions: Record<ModuleKey, string> = {
    inventory: `Keep ${currency} holdings near your target.`,
    spreads: 'Adjust quote margins as market prices move.',
    rebalance: `Sell excess ${currency} to your wallet or an invited buyer.`,
    liquidity: `Keep more ${settlement} liquid in market hours; let more earn yield in Aave off-hours.`,
    curve: `Lean Swap quotes on the vault's ${currency} share, priced across each trade.`,
  }
  return (
    <div className="space-y-4">
      <Card>
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
      <Card
        title="Your modules"
        action={<span className="text-muted text-xs">Saved settings</span>}
      >
        <div className="divide-line-soft divide-y">
          {modulesFor(true).map((key, i) => (
            <button
              key={key}
              onClick={() => onConfigure(key)}
              className="hover:bg-canvas flex w-full items-center gap-4 rounded-lg py-4 text-left transition first:pt-1 last:pb-1"
            >
              <span
                aria-hidden
                className="bg-accent-tint flex size-9 shrink-0 items-center justify-center rounded-lg text-sm font-bold text-accent"
              >
                0{i + 1}
              </span>
              <span className="min-w-0 flex-1">
                <span className="block text-sm font-bold">
                  {moduleNames[key]}
                </span>
                <span className="text-muted mt-1 block text-xs">
                  {descriptions[key]}
                </span>
              </span>
              <span
                className={`rounded-full px-2.5 py-1 text-xs ${view.config[key].enabled ? 'bg-accent-tint text-accent' : 'bg-hover text-muted'}`}
              >
                {view.config[key].enabled ? 'Enabled' : 'Off'}
              </span>
              <span aria-hidden className="text-muted">
                ›
              </span>
            </button>
          ))}
        </div>
        {config.rebalance.enabled && state.current && (
          <p className="border-line-soft text-muted mt-4 border-t pt-3 text-sm">
            Spot rebalancing: {view.status?.rebalance_status}
          </p>
        )}
      </Card>
      {view.config.curve.enabled && (
        <Card title="Inventory curve">
          <CurveNow
            view={view}
            now={now}
            currency={currency}
            settlement={settlement}
          />
        </Card>
      )}
      {(view.config.liquidity?.enabled || view.status?.liquidity) && (
        <LiquidityScheduleLive view={view} now={now} settlement={settlement} />
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

/** The Overview of a bot that trades from its own wallet: its one module is
 * the inventory curve, so the page is the curve's. */
function WalletOverview({
  view,
  now,
  currency,
  settlement,
  onConfigure,
  onDecisions,
}: {
  view: ModulesView
  now: number
  currency: string
  settlement: string
  onConfigure: (key: ModuleKey) => void
  onDecisions: () => void
}) {
  const state = curveOverviewState(view, now)
  const reading = freshReading(view.status?.curve, now)
  const quotes = view.status?.curve?.quotes ?? []
  const flat = quotes.filter((q) => q.flat_reason !== null).length
  const on = view.config.curve.enabled
  return (
    <div className="space-y-4">
      <Card>
        <div className="flex flex-wrap items-start justify-between gap-4">
          <div>
            <p className="text-muted mb-2 text-xs font-bold uppercase tracking-wider">
              Right now
            </p>
            <h3 className="text-xl font-bold">{state.title}</h3>
            <p className="text-muted mt-2 max-w-xl text-sm">{state.message}</p>
          </div>
          <span className="bg-hover rounded-full px-3 py-1 text-xs font-bold">
            {state.pill}
          </span>
        </div>
        <div className="border-line-soft mt-6 border-t pt-6">
          <CurveNow
            view={view}
            now={now}
            currency={currency}
            settlement={settlement}
          />
        </div>
      </Card>
      <Card
        title="Your modules"
        action={<span className="text-muted text-xs">Saved settings</span>}
      >
        <button
          onClick={() => onConfigure('curve')}
          className="hover:bg-canvas flex w-full items-center gap-4 rounded-lg py-1 text-left transition"
        >
          <span
            aria-hidden
            className="bg-accent-tint flex size-9 shrink-0 items-center justify-center rounded-lg text-sm font-bold text-accent"
          >
            01
          </span>
          <span className="min-w-0 flex-1">
            <span className="block text-sm font-bold">{moduleNames.curve}</span>
            <span className="text-muted mt-1 block text-xs">
              Lean Swap quotes on this wallet&apos;s {currency} share, priced
              across each trade.
            </span>
          </span>
          <span
            className={`rounded-full px-2.5 py-1 text-xs ${on ? 'bg-accent-tint text-accent' : 'bg-hover text-muted'}`}
          >
            {on ? 'Enabled' : 'Off'}
          </span>
          <span aria-hidden className="text-muted">
            ›
          </span>
        </button>
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
                  'Wallet value',
                  reading
                    ? `${reading.value.toLocaleString(undefined, { maximumFractionDigits: 0 })} ${settlement}`
                    : '—',
                ],
                ['Venue fee, last request', reading ? `${reading.fee_bps} bps` : '—'],
                ['Answered flat', quotes.length ? `${flat} of the last ${quotes.length}` : '—'],
              ] as const
            ).map(([label, value]) => (
              <div key={label}>
                <dt className="text-muted text-xs">{label}</dt>
                <dd className="font-bold tabular-nums">{value}</dd>
              </div>
            ))}
          </dl>
          <p className="text-muted text-xs">
            Holdings are this wallet&apos;s raw balances valued at the reference
            mid, with this side&apos;s open quotes counted as already moved.
            Quotes are calculations, not completed trades.
          </p>
          <Button onClick={onDecisions}>Open quote history</Button>
        </div>
      </Disclosure>
    </div>
  )
}

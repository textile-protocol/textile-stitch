import { type ModulesView } from '../modules'
import {
  moduleKeys,
  moduleNames,
  moduleOverviewState,
  percentage,
  type ModuleKey,
} from '../modulePresentation'
import { AllocationBar, Disclosure } from './ModuleControls'
import DynamicSpreadsLive from './DynamicSpreadsLive'
import { Button, Card } from './ui'

export default function ModulesOverview({
  view,
  now,
  currency,
  onConfigure,
  onSimulate,
  onDecisions,
}: {
  view: ModulesView
  now: number
  currency: string
  onConfigure: (key: ModuleKey) => void
  onSimulate: () => void
  onDecisions: () => void
}) {
  const state = moduleOverviewState(view, now)
  const config = view.status?.config ?? view.config
  const latest = view.status?.decisions.at(-1)?.decision
  const current = state.current ? latest : undefined
  const share = current?.inventory_bps
  const margins = current && !current.blocked
  const shadow = config.mode === 'shadow'
  const differs =
    !!view.status &&
    JSON.stringify(view.status.config) !== JSON.stringify(view.config)
  const descriptions: Record<ModuleKey, string> = {
    inventory: `Keep ${currency} holdings near your target.`,
    spreads: 'Adjust quote margins as market prices move.',
    rebalance: `Sell excess ${currency} to your wallet or an invited buyer.`,
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
          {moduleKeys.map((key, i) => (
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
      <div className="flex flex-wrap items-center justify-between gap-3 px-1">
        <p className="text-muted text-sm">
          See how different settings would have performed.
        </p>
        <Button onClick={onSimulate}>
          Try a historical simulation <span aria-hidden>↗</span>
        </Button>
      </div>
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

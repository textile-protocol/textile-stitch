import {
  dynamicSpreadAdditions,
  dynamicSpreadState,
  type ModulesView,
} from '../modules'
import { Banner, Card } from './ui'

const bps = (v: number | null | undefined) =>
  v == null ? '—' : `${v.toLocaleString()} bps`
const side = (v: number | null | undefined) =>
  v === null ? 'Paused / off' : bps(v)
const addition = (v: number | null | undefined) =>
  v === undefined ? '—' : v === null ? 'Side off' : `+${bps(v)}`
const price = (v: number | null | undefined) =>
  v == null ? '—' : v.toLocaleString(undefined, { maximumSignificantDigits: 7 })
const time = (at: number) => new Date(at * 1000).toLocaleTimeString()
const adjustment = (
  before: number | null | undefined,
  after: number | null | undefined
) =>
  before === undefined || after === undefined
    ? '—'
    : before === null || after === null
      ? 'Side off'
      : `${after - before > 0 ? '+' : ''}${after - before} bps`

export default function DynamicSpreadsLive({
  view,
  now,
}: {
  view: ModulesView
  now: number
}) {
  const { status } = view
  const last = status?.decisions.at(-1)
  const inputs = last?.inputs
  const window = inputs?.spread_window
  const config = status?.config ?? view.config
  const health = dynamicSpreadState(view, now)
  const current = health.current && !!last
  const complete = current && !last.decision.blocked
  const added = dynamicSpreadAdditions(complete ? last : undefined)
  const inventoryAware =
    config.spreads.inventory_aware && config.inventory.enabled
  const points = (status?.decisions ?? [])
    .filter((o) => o.decision.at <= now)
    .map((o) => ({ ...o, additions: dynamicSpreadAdditions(o) }))
  const first = points[0]?.decision.at ?? 0
  const end = points.at(-1)?.decision.at ?? first
  const max = Math.max(
    1,
    config.spreads.max_extra_bps,
    ...points.flatMap((o) => [o.additions.buy ?? 0, o.additions.sell ?? 0])
  )
  const segments = (key: 'buy' | 'sell') =>
    points.reduce<(typeof points)[]>((lines, o) => {
      const previous = lines.at(-1)?.at(-1)
      if (o.additions[key] == null) return [...lines, []]
      if (!previous || o.decision.at - previous.decision.at > 10)
        return [...lines, [o]]
      return [...lines.slice(0, -1), [...lines.at(-1)!, o]]
    }, [])
  const coordinates = (line: typeof points, key: 'buy' | 'sell') =>
    line
      .map(
        (o) =>
          `${20 + ((o.decision.at - first) / Math.max(1, end - first)) * 660},${125 - (o.additions[key]! / max) * 100}`
      )
      .join(' ')
  const recent = points
    .filter((o, i, all) => {
      const previous = all[i - 1]
      return (
        !previous ||
        o.decision.blocked !== previous.decision.blocked ||
        o.additions.buy !== previous.additions.buy ||
        o.additions.sell !== previous.additions.sell ||
        o.decision.volatility_bps !== previous.decision.volatility_bps ||
        o.decision.buy_bps !== previous.decision.buy_bps ||
        o.decision.sell_bps !== previous.decision.sell_bps
      )
    })
    .slice(-5)
    .reverse()
  const modeDiffers =
    status && JSON.stringify(view.config) !== JSON.stringify(status.config)
  if (!last)
    return (
      <Card title="Spread readings">
        <p className="text-sm">{health.message}</p>
      </Card>
    )
  if (last && !inputs)
    return (
      <Card title="Detailed spread readings">
        <p className="text-sm">
          The last bot report uses the older format. Update the bot on its card
          above to collect the price window and spread breakdown. Updating only
          the panel is not enough.
        </p>
        <p className="text-muted mt-3 text-xs">
          Existing quote decisions remain available in the decision history.
          Missing detail is not treated as zero.
        </p>
      </Card>
    )
  return (
    <Card
      title="Dynamic spreads · live monitor"
      action={
        <span
          className={`rounded-full px-3 py-1 text-xs font-bold ${complete ? 'bg-hover text-ink' : 'bg-canvas text-muted'}`}
        >
          {health.label}
        </span>
      }
    >
      <p className="mb-4 text-sm">{health.message}</p>
      {modeDiffers && (
        <Banner tone="warning">
          Saved settings differ from this runtime snapshot. This monitor uses
          the running configuration; check whether the restart completed.
        </Banner>
      )}
      {!current && last && (
        <p className="text-muted mb-3 text-xs">
          Last evaluation: {new Date(last.decision.at * 1000).toLocaleString()}.
          The chart below is historical; current values are withheld.
        </p>
      )}
      {status?.quote_status && !current && (
        <p className="text-muted mb-3 text-xs">
          Last runtime status:{' '}
          {new Date(status.quote_status.at * 1000).toLocaleString()}
        </p>
      )}
      <div className="grid gap-4 sm:grid-cols-3">
        <div className="bg-canvas rounded-xl p-4">
          <p className="text-muted text-xs">
            Dynamic addition · vault buys / sells
          </p>
          <p className="my-2 text-xl font-bold">
            {addition(added.buy)} / {addition(added.sell)}
          </p>
          <p className="text-muted text-xs">
            {complete
              ? `${bps(last.decision.volatility_bps)} buffer before inventory weighting. `
              : ''}
            Cap: {config.spreads.max_extra_bps} bps per side · 100 bps = 1%
          </p>
        </div>
        <div className="bg-canvas rounded-xl p-4">
          <p className="text-muted text-xs">
            Observed price range · {config.spreads.window_secs}s window
          </p>
          <p className="my-2 text-xl font-bold">
            {current && window?.low && window.high
              ? `${((window.high / window.low - 1) * 100).toFixed(3)}%`
              : '—'}
          </p>
          <p className="text-muted text-xs">
            {current
              ? `${price(window?.low)} – ${price(window?.high)}`
              : 'Waiting for current observations'}{' '}
            · settlement per corridor token
          </p>
        </div>
        <div className="bg-canvas rounded-xl p-4">
          <p className="text-muted text-xs">Price history collected</p>
          <p className="my-2 text-xl font-bold">
            {current && window
              ? `${window.samples} ${window.samples === 1 ? 'sample' : 'samples'}`
              : '—'}
          </p>
          <p className="text-muted text-xs">
            {current && window
              ? `${window.history_secs}s of observed history · ${config.spreads.warmup_secs}s required to warm up`
              : 'New source timestamps advance the warmup'}
          </p>
        </div>
      </div>
      <div className="mt-5 overflow-x-auto">
        <table className="w-full text-left text-sm">
          <caption className="mb-2 text-left font-bold">
            How the current quote policy is calculated
          </caption>
          <thead>
            <tr className="text-muted">
              <th className="py-2 pr-3">Spread component</th>
              <th className="pr-3">Vault buys</th>
              <th>Vault sells</th>
            </tr>
          </thead>
          <tbody>
            {[
              [
                'Base spread',
                side(current ? inputs?.base_buy_bps : undefined),
                side(current ? inputs?.base_sell_bps : undefined),
              ],
              [
                'Inventory adjustment',
                current
                  ? adjustment(inputs?.base_buy_bps, inputs?.inventory_buy_bps)
                  : '—',
                current
                  ? adjustment(
                      inputs?.base_sell_bps,
                      inputs?.inventory_sell_bps
                    )
                  : '—',
              ],
              ['Dynamic addition', addition(added.buy), addition(added.sell)],
              [
                config.mode === 'shadow'
                  ? 'Proposed spread (shadow)'
                  : 'Final quote policy',
                side(current ? last.decision.buy_bps : undefined),
                side(current ? last.decision.sell_bps : undefined),
              ],
            ].map(([label, buy, sell]) => (
              <tr key={label} className="border-line-soft border-t">
                <td className="py-2 pr-3">{label}</td>
                <td className="pr-3">{buy}</td>
                <td>{sell}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p className="text-muted mt-2 text-xs">
        Price range × {config.spreads.multiplier} multiplier, rounded up to
        whole basis points and capped at {config.spreads.max_extra_bps} bps.{' '}
        {inventoryAware
          ? 'Inventory weighting keeps the full buffer on the side moving away from target and reduces it on the side moving toward target. Each side stays within the cap.'
          : config.spreads.inventory_aware
            ? 'Inventory balancing is off, so additions remain equal before side limits.'
            : 'Both sides receive the same buffer before side limits.'}{' '}
        Floors, spread limits and paused sides can affect the final policy. This
        is quote telemetry, not a record of executed trades.
      </p>
      {inventoryAware && current && last.decision.inventory_bps !== null && (
        <p className="mt-2 text-sm">
          Corridor inventory: {(last.decision.inventory_bps / 100).toFixed(2)}%{' '}
          · target: {(config.inventory.target_bps / 100).toFixed(2)}% ·{' '}
          {last.decision.inventory_bps > config.inventory.target_bps
            ? 'Favoring corridor sales'
            : last.decision.inventory_bps < config.inventory.target_bps
              ? 'Favoring corridor purchases'
              : 'At target: equal buffers'}
        </p>
      )}
      {current && inputs && (
        <p className="text-muted mt-2 text-xs">
          Evaluation {time(last.decision.at)} · source price{' '}
          {time(inputs.price_at)} · vault balances {time(inputs.balances_at)} ·
          panel refreshes every 5s
        </p>
      )}
      {points.length > 0 && (
        <>
          <div className="mt-5 flex justify-between gap-2 text-sm">
            <p className="font-bold">Recent dynamic additions</p>
            <span className="text-muted">
              {config.mode === 'shadow'
                ? 'Shadow proposals'
                : 'Recorded evaluations'}
            </span>
          </div>
          <svg
            viewBox="0 0 700 155"
            role="img"
            aria-label="Recent buy and sell dynamic spread additions in basis points; gaps indicate unavailable or paused sides"
            className="mt-2 w-full"
          >
            <line
              x1="20"
              x2="680"
              y1="125"
              y2="125"
              stroke="currentColor"
              opacity="0.15"
            />
            <text x="20" y="15" fill="currentColor" fontSize="11" opacity="0.6">
              {max} bps
            </text>
            <text
              x="20"
              y="146"
              fill="currentColor"
              fontSize="11"
              opacity="0.6"
            >
              0 bps
            </text>
            {(['buy', 'sell'] as const).flatMap((key) =>
              segments(key)
                .filter((line) => line.length > 0)
                .map((line, i) =>
                  line.length === 1 ? (
                    <circle
                      key={`${key}-${i}`}
                      cx={
                        20 +
                        ((line[0]!.decision.at - first) /
                          Math.max(1, end - first)) *
                          660
                      }
                      cy={125 - (line[0]!.additions[key]! / max) * 100}
                      r="2"
                      fill={key === 'buy' ? 'var(--tx-accent)' : 'currentColor'}
                    />
                  ) : (
                    <polyline
                      key={`${key}-${i}`}
                      points={coordinates(line, key)}
                      stroke={
                        key === 'buy' ? 'var(--tx-accent)' : 'currentColor'
                      }
                      strokeDasharray={key === 'sell' ? '5 4' : undefined}
                      strokeWidth="2.5"
                      fill="none"
                    />
                  )
                )
            )}
          </svg>
          <p className="text-muted text-xs">
            Solid accent: vault buys · dashed: vault sells. {time(first)} –{' '}
            {time(end)} · last {points.length} evaluations (up to 200). Gaps
            indicate unavailable or paused sides.
          </p>
          <div className="mt-4 overflow-x-auto">
            <table className="w-full text-left text-xs">
              <caption className="mb-2 text-left text-sm font-bold">
                Recent changes
              </caption>
              <thead className="text-muted">
                <tr>
                  <th className="py-2 pr-3">Time</th>
                  <th className="pr-3">Dynamic buy / sell</th>
                  <th className="pr-3">Buy / sell policy</th>
                  <th>State</th>
                </tr>
              </thead>
              <tbody>
                {recent.map((o) => (
                  <tr key={o.decision.at} className="border-line-soft border-t">
                    <td className="py-2 pr-3">{time(o.decision.at)}</td>
                    <td className="pr-3">
                      {addition(o.additions.buy)} / {addition(o.additions.sell)}
                    </td>
                    <td className="pr-3">
                      {side(o.decision.buy_bps)} / {side(o.decision.sell_bps)}
                    </td>
                    <td>
                      {o.decision.blocked
                        ? 'Blocked'
                        : config.mode === 'shadow'
                          ? 'Shadow'
                          : 'Evaluated'}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
    </Card>
  )
}

import { formatAtomic } from '../format'
import type { ModulesView } from '../modules'
import {
  liquiditySetupHint,
  liquidityState,
  percentage,
  shareBps,
} from '../modulePresentation'
import { moduleMode } from '../moduleModes'
import { Banner, Card } from './ui'

const when = (at: number) =>
  new Date(at * 1000).toLocaleString(undefined, {
    weekday: 'short',
    hour: '2-digit',
    minute: '2-digit',
  })
type Stat = [label: string, raw: string | null, note: string | undefined]
const actionName = (action: string) =>
  action.replace(/_/g, ' ').replace(/^./, (c) => c.toUpperCase())

/** Live reading of the liquidity schedule. Shows the last reading when it is
 * stale, labeled with its time; never fills missing values with zero. */
export default function LiquidityScheduleLive({
  view,
  now,
  settlement,
}: {
  view: ModulesView
  now: number
  settlement: string
}) {
  const l = view.status?.liquidity ?? null
  const state = liquidityState(view, now)
  const shadow =
    moduleMode(view.status?.config ?? view.config, 'liquidity') === 'shadow'
  const amount = (raw: string | null) =>
    raw === null ? '—' : formatAtomic(raw, view.settlement_decimals)
  const ofFree = (raw: string | null) => {
    const share = shareBps(raw, l?.free ?? null)
    return share === null ? undefined : `${percentage(share)} of free`
  }
  const hint = l && liquiditySetupHint(l)
  const stats: Stat[] = l
    ? [
        ['On-chain floor', l.floor, ofFree(l.floor)],
        ['Scheduled target', l.target, ofFree(l.target)],
        ['Liquid', l.liquid, ofFree(l.liquid)],
        ['Free (liquid + Aave)', l.free, undefined],
        ['Committed to quotes', l.committed, undefined],
        ...(l.quote_cap === null
          ? []
          : [
              [
                shadow ? 'Would cap new quotes at' : 'New quotes capped at',
                l.quote_cap,
                undefined,
              ] satisfies Stat,
            ]),
      ]
    : []
  return (
    <Card
      title={`Liquidity schedule · ${shadow ? 'preview' : 'live'}`}
      action={
        <span
          className={`rounded-full px-3 py-1 text-xs font-bold ${state.current ? 'bg-hover text-ink' : 'bg-canvas text-muted'}`}
        >
          {state.label}
        </span>
      }
    >
      <p className="text-sm">{state.message}</p>
      {l && (
        <div className="mt-4 space-y-4">
          <p className="text-sm">
            <strong>{l.market_hours ? 'Market hours' : 'Off-hours'}</strong>
            {l.next_change_at !== null &&
              ` · ${l.market_hours ? 'off-hours start' : 'market hours start'} ${when(l.next_change_at)} your time`}
          </p>
          <dl className="grid grid-cols-2 gap-3 sm:grid-cols-3">
            {stats.map(([label, raw, note]) => (
              <div key={label} className="bg-canvas rounded-xl p-3">
                <dt className="text-muted text-xs">{label}</dt>
                <dd className="mt-1 font-bold tabular-nums">{amount(raw)}</dd>
                {note && <dd className="text-muted text-xs">{note}</dd>}
              </div>
            ))}
          </dl>
          <p className="text-muted text-xs">
            Amounts in {settlement}.{' '}
            {state.current
              ? `Reading at ${new Date(l.at * 1000).toLocaleTimeString()}.`
              : `Last reading ${new Date(l.at * 1000).toLocaleString()}; may be out of date.`}
          </p>
          {l.in_flight && (
            <Banner tone="info">
              {shadow ? 'Recorded: ' : 'Transaction in progress: '}
              {l.in_flight}
            </Banner>
          )}
          {hint && <Banner tone="warning">{hint}</Banner>}
          {l.actions.length > 0 && (
            <div>
              <p className="text-sm font-bold">
                {shadow ? 'Recent proposals' : 'Recent actions'}
              </p>
              <ul className="divide-line-soft mt-2 divide-y text-xs">
                {l.actions
                  .slice(-5)
                  .reverse()
                  .map((a, i) => (
                    <li
                      key={`${a.at}:${i}`}
                      className="flex flex-wrap gap-x-3 py-2"
                    >
                      <span className="text-muted tabular-nums">
                        {when(a.at)}
                      </span>
                      <span className="font-bold">{actionName(a.action)}</span>
                      <span className={a.ok ? 'text-muted' : 'text-danger'}>
                        {a.ok ? 'OK' : 'Failed'}
                      </span>
                      {a.detail && (
                        <span className="text-muted min-w-0 break-all">
                          {a.detail}
                        </span>
                      )}
                    </li>
                  ))}
              </ul>
            </div>
          )}
        </div>
      )}
    </Card>
  )
}

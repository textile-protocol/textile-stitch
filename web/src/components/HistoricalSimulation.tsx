import { useState } from 'react'
import { api } from '../api'
import {
  downloadJson,
  simulationAmount,
  type HistoricalSimulation,
  type ModulesConfig,
} from '../modules'
import { Banner, Button, Card, Field, Input, Select, Toggle } from './ui'

export function HistoricalSimulationForm({
  name,
  config,
  settlementDecimals,
  corridorDecimals,
  busy,
  onStart,
  onResult,
  onError,
  onDone,
}: {
  name: string
  config: ModulesConfig
  settlementDecimals: number
  corridorDecimals: number
  busy: boolean
  onStart: () => void
  onResult: (result: HistoricalSimulation) => void
  onError: (error: string) => void
  onDone: () => void
}) {
  const [period, setPeriod] = useState('24')
  const [from, setFrom] = useState('')
  const [to, setTo] = useState('')
  const [cost, setCost] = useState('0')
  const [dealer, setDealer] = useState(false)
  const [slippage, setSlippage] = useState('30')
  const [depth, setDepth] = useState('')
  async function run() {
    onStart()
    try {
      // A stable minute boundary also lets repeated drafts reuse the collection.
      const end =
        period === 'custom'
          ? Math.floor(new Date(to).getTime() / 1000)
          : Math.floor(Date.now() / 60000) * 60 - 600
      const start =
        period === 'custom'
          ? Math.floor(new Date(from).getTime() / 1000)
          : end - Number(period) * 3600
      if (!Number.isFinite(start) || !Number.isFinite(end))
        throw new Error('Choose a start and end time.')
      if (dealer && (!/^\d+$/.test(slippage) || Number(slippage) >= 1000))
        throw new Error('Enter dealer slippage from 0 to 999 basis points.')
      onResult(
        await api.simulateModuleHistory(name, config, {
          from: start,
          to: end,
          cost_per_trade: simulationAmount(cost, settlementDecimals),
          dealer_scenario: dealer
            ? {
                slippage_bps: Number(slippage),
                max_corridor_per_observation: simulationAmount(
                  depth,
                  corridorDecimals
                ),
              }
            : null,
        })
      )
    } catch (e) {
      onError(String(e))
    } finally {
      onDone()
    }
  }
  return (
    <Card title="Test on your vault’s history">
      <p className="text-muted mb-4 text-sm">
        Collect archived starting balances, Textile reference prices and your
        vault’s indexed swaps. Compare your draft modules with the current quote
        settings, using the same starting capital.
      </p>
      <div className="grid gap-4 sm:grid-cols-2">
        <Field
          label="Historical period"
          hint="Recent periods end 10 minutes ago to allow indexing."
        >
          <Select
            value={period}
            disabled={busy}
            onChange={(e) => setPeriod(e.target.value)}
          >
            <option value="6">Last 6 hours</option>
            <option value="24">Last 24 hours</option>
            <option value="72">Last 3 days</option>
            <option value="custom">Custom period</option>
          </Select>
        </Field>
        <Field
          label="Cost per simulated fill"
          hint="Settlement tokens, including your gas/fee estimate. Zero means returns before these costs."
        >
          <Input
            inputMode="decimal"
            value={cost}
            disabled={busy}
            onChange={(e) => setCost(e.target.value)}
          />
        </Field>
        {period === 'custom' && (
          <>
            <Field label="Start (your local time)">
              <Input
                type="datetime-local"
                value={from}
                disabled={busy}
                onChange={(e) => setFrom(e.target.value)}
              />
            </Field>
            <Field
              label="End (your local time)"
              hint="Up to 30 days; at least 5 minutes before now."
            >
              <Input
                type="datetime-local"
                value={to}
                disabled={busy}
                onChange={(e) => setTo(e.target.value)}
              />
            </Field>
          </>
        )}
      </div>
      <details className="my-4 text-sm">
        <summary className="cursor-pointer font-bold">
          Optional spot execution scenario
        </summary>
        <p className="text-muted my-3">
          Historical executable dealer quotes are not stored. By default, spot
          rebalancing receives no fills. You can model hypothetical liquidity to
          explore its impact; this does not establish that a dealer could have
          executed those trades.
        </p>
        <Toggle
          checked={dealer}
          onChange={setDealer}
          disabled={busy}
          label="Model hypothetical dealer liquidity"
        />
        {dealer && (
          <div className="mt-3 grid gap-4 sm:grid-cols-2">
            <Field
              label="Dealer discount (basis points)"
              hint="30 bps = a sale 0.30% below the reference price."
            >
              <Input
                value={slippage}
                inputMode="numeric"
                disabled={busy}
                onChange={(e) => setSlippage(e.target.value)}
              />
            </Field>
            <Field
              label="Dealer depth per price observation"
              hint="Corridor tokens available to sell. Assumed to replenish on each fresh observation."
            >
              <Input
                value={depth}
                inputMode="decimal"
                disabled={busy}
                onChange={(e) => setDepth(e.target.value)}
              />
            </Field>
          </div>
        )}
      </details>
      <Button variant="primary" busy={busy} onClick={() => void run()}>
        Collect data and run
      </Button>
      <p className="text-muted mt-3 text-xs">
        {busy
          ? 'Collecting historical data and comparing portfolios…'
          : 'Read-only simulation. Nothing is saved or traded. Up to 10,000 events per run; choose a shorter period for busy vaults.'}
      </p>
    </Card>
  )
}

const signed = (value: number) =>
  `${value > 0 ? '+' : ''}${value.toLocaleString(undefined, { maximumFractionDigits: 2 })}`
const date = (at: number) => new Date(at * 1000).toLocaleString()
export function HistoricalAnalysis({
  result,
}: {
  result: HistoricalSimulation
}) {
  const { coverage: c, source: s, analysis: a } = result
  const coveragePct = c.total_seconds
    ? ((100 * c.fresh_seconds) / c.total_seconds).toFixed(1)
    : '0'
  return (
    <>
      <Card
        title="What changed with your modules"
        action={
          <Button
            onClick={() =>
              downloadJson('stitch-collected-history.json', result.dataset)
            }
          >
            Export collected dataset
          </Button>
        }
      >
        {result.options.dealer_scenario && (
          <Banner tone="warning">
            Hypothetical dealer execution is included. These are scenario
            returns, not observed hedge returns.
          </Banner>
        )}
        <div className="my-4 grid grid-cols-2 gap-4 lg:grid-cols-4">
          {[
            ['Return difference', `${signed(a.return_delta_pp)} pp`],
            ['Drawdown difference', `${signed(a.drawdown_delta_bps / 100)} pp`],
            [
              'Peak currency exposure',
              `${signed(a.inventory_delta_bps / 100)} pp`,
            ],
            ['Customer fills', signed(a.fill_delta)],
          ].map(([label, value]) => (
            <div key={label}>
              <p className="text-muted text-xs">{label}</p>
              <p className="mt-1 text-xl font-bold">{value}</p>
            </div>
          ))}
        </div>
        <p className="text-muted mb-3 text-xs">
          Differences versus baseline. Lower drawdown and currency exposure mean
          less modeled risk. Fewer fills may also mean lost business.
        </p>
        <p className="text-sm">{a.conclusion}</p>
      </Card>
      <Card title="Data used in this run">
        <dl className="grid gap-3 text-sm sm:grid-cols-2">
          {[
            ['Period', `${date(s.from)} – ${date(s.to)}`],
            ['Reference source', s.price_source],
            [
              'Fresh price coverage',
              `${coveragePct}% · ${c.price_observations.toLocaleString()} observations`,
            ],
            [
              'Vault swaps',
              `${c.replayed_trades} of ${c.observed_trades} evaluated · ${c.skipped_stale_trades} skipped for stale prices`,
            ],
            [
              'Largest observation gap',
              `${c.max_price_gap_secs.toLocaleString()} seconds`,
            ],
            [
              'Starting vault balances',
              `Archived block ${s.snapshot.block_number} · ${date(s.snapshot.at)}`,
            ],
            ['Trade index covers through', date(s.indexed_through)],
            ['Collected', date(s.collected_at)],
          ].map(([label, value]) => (
            <div key={label}>
              <dt className="text-muted text-xs">{label}</dt>
              <dd className="mt-1 break-words">{value}</dd>
            </div>
          ))}
        </dl>
        <p className="text-muted mt-4 text-xs">
          Reference prices may differ from your operator feed. Observed fill
          prices are assumed customer limits; actual willingness to trade at
          different prices is unknown. Price freshness uses this bot’s{' '}
          {s.staleness_secs}s limit. Gaps are not filled with future
          observations.
        </p>
      </Card>
    </>
  )
}

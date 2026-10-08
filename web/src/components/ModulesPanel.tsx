import ManualSales from './ManualSales'
import { useEffect, useState } from 'react'
import { api } from '../api'
import { poll } from '../poll'
import ModulesOverview from './ModulesOverview'
import ModuleParameters from './ModuleParameters'
import ScrollTabs from './ScrollTabs'
import { moduleParameterErrors, type ModuleKey } from '../modulePresentation'
import {
  HistoricalSimulationForm,
  HistoricalAnalysis,
} from './HistoricalSimulation'
import { formatAtomic } from '../format'
import {
  downloadJson,
  equityCoordinates,
  isWarmingUp,
  type ModulesConfig,
  type ModulesView,
  type SimulationReport,
  type HistoricalSimulation,
} from '../modules'
import {
  Banner,
  Button,
  Card,
  ErrorState,
  Field,
  Input,
  Loading,
  Select,
} from './ui'

const percent = (bps: number | null) =>
  bps === null
    ? '—'
    : `${(bps / 100).toLocaleString(undefined, { maximumFractionDigits: 2 })}%`
const clock = (at: number) => new Date(at * 1000).toLocaleString()
const spread = (bps: number | null) =>
  bps === null ? 'Paused' : `${bps.toLocaleString()} bps`
type Section = 'sales' | 'overview' | 'parameters' | 'simulation' | 'decisions'

export default function ModulesPanel({
  name,
  editable,
  pair,
}: {
  name: string
  editable: boolean
  pair?: string
}) {
  const [now, setNow] = useState(() => Date.now() / 1000)
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now() / 1000), 1000)
    return () => clearInterval(timer)
  }, [])
  const [view, setView] = useState<ModulesView | null>(null)
  const [draftRevision, setDraftRevision] = useState('')
  const [draft, setDraft] = useState<ModulesConfig | null>(null)
  const [section, setSection] = useState<Section>('overview')
  const [error, setError] = useState<string | null>(null)
  const [note, setNote] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [dataset, setDataset] = useState<unknown>(null)
  const [fileName, setFileName] = useState('')
  const [report, setReport] = useState<SimulationReport | null>(null)
  const [historical, setHistorical] = useState<HistoricalSimulation | null>(
    null
  )
  const [decisionFilter, setDecisionFilter] = useState('all')
  const [selectedModule, setSelectedModule] = useState<ModuleKey>('inventory')

  useEffect(() => {
    let cancelled = false
    async function load() {
      try {
        const next = await api.modules(name)
        if (cancelled) return
        setView(next)
        setDraft((current) => current ?? next.config)
        setDraftRevision((current) => current || next.revision)
      } catch (e) {
        if (!cancelled) setError(String(e))
      }
    }
    const stop = poll(load, 5000, { immediate: true })
    return () => {
      cancelled = true
      stop()
    }
  }, [name])

  if (!view || !draft)
    return error ? <ErrorState error={error} /> : <Loading what="modules" />
  const changed = JSON.stringify(draft) !== JSON.stringify(view.config)
  const status = view.status
  const parameterErrors = moduleParameterErrors(draft)
  const valid = parameterErrors.length === 0
  const [currency = 'Corridor currency', settlement = 'Settlement currency'] =
    pair?.split(' / ') ?? []

  async function save() {
    if (!view || !draft || !editable || !valid || busy) return
    setBusy(true)
    setError(null)
    setNote(null)
    try {
      const result = await api.saveModules(name, draftRevision, draft)
      const next = await api.modules(name)
      setView(next)
      setDraft(next.config)
      setDraftRevision(next.revision)
      setNote(result.message)
      if (result.restartError)
        setError(
          `Configuration saved, but restart failed: ${result.restartError}`
        )
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }
  async function simulate() {
    if (!draft || !dataset || !valid || busy) return
    setHistorical(null)
    setBusy(true)
    setError(null)
    try {
      setReport(await api.simulateModules(name, draft, dataset))
    } catch (e) {
      setError(String(e))
      setReport(null)
    } finally {
      setBusy(false)
    }
  }
  async function importFile(file: File | undefined) {
    setHistorical(null)
    setReport(null)
    setDataset(null)
    setFileName('')
    setError(null)
    if (!file) return
    if (file.size > 1_900_000) {
      setError('Historical data must be smaller than 1.9 MB.')
      return
    }
    try {
      setDataset(JSON.parse(await file.text()) as unknown)
      setFileName(file.name)
    } catch {
      setError('The file must contain a valid historical JSON dataset.')
    }
  }
  return (
    <section aria-label="Module workspace" className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-xl font-bold">Modules</h2>
          <p className="text-muted text-sm">
            Manage your holdings and quote margins.
          </p>
        </div>
      </div>
      <ScrollTabs
        label="Module sections"
        value={section}
        onChange={setSection}
        items={[
          { value: 'overview', label: 'Overview' },
          { value: 'parameters', label: 'Parameters' },
          { value: 'sales', label: 'Manual sale' },
          { value: 'simulation', label: 'Historical simulation' },
          { value: 'decisions', label: 'Decisions' },
        ]}
      />
      {error && (
        <Banner tone="danger" onDismiss={() => setError(null)}>
          {error}
        </Banner>
      )}
      {note && (
        <Banner tone="info" onDismiss={() => setNote(null)}>
          {note}
        </Banner>
      )}
      {changed && (
        <Banner tone="info">
          You have unsaved changes. Live settings stay unchanged until you save.
        </Banner>
      )}
      {section === 'sales' && (
        <ManualSales
          name={name}
          view={view}
          editable={editable}
          currency={currency}
          settlement={settlement}
          onConfigure={() => {
            setSelectedModule('rebalance')
            setSection('parameters')
          }}
        />
      )}
      {section === 'overview' && (
        <ModulesOverview
          view={view}
          now={now}
          currency={currency}
          onConfigure={(key) => {
            if (key === 'rebalance') {
              setSection('sales')
              return
            }
            setSelectedModule(key)
            setSection('parameters')
          }}
          onSimulate={() => setSection('simulation')}
          onDecisions={() => setSection('decisions')}
        />
      )}
      {section === 'parameters' && (
        <>
          <fieldset disabled={busy} className="min-w-0">
            <ModuleParameters
              draft={draft}
              onChange={setDraft}
              selected={selectedModule}
              onSelect={setSelectedModule}
              currency={currency}
              settlement={settlement}
            />
          </fieldset>
          {!valid && (
            <Banner tone="warning">
              <div role="alert">
                <p className="mb-1 font-bold">A few settings need attention</p>
                {parameterErrors.map((message) => (
                  <p key={message} className="mt-1">
                    {message}
                  </p>
                ))}
              </div>
            </Banner>
          )}
          <div className="border-line bg-surface z-10 rounded-xl border p-4 shadow-sm sm:sticky sm:bottom-3">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div>
                <p className="text-sm font-bold">
                  {!valid
                    ? 'Check your settings before continuing'
                    : changed
                      ? 'Your changes are ready to test'
                      : 'Settings are up to date'}
                </p>
                <p className="text-muted mt-1 text-xs">
                  {editable
                    ? 'Saving restarts a running bot. A stopped bot stays stopped.'
                    : 'Read-only access. You can explore a draft and simulate it, but cannot save.'}
                </p>
              </div>
              <div className="flex flex-wrap gap-2">
                {changed && (
                  <Button
                    disabled={busy}
                    variant="ghost"
                    onClick={() => {
                      setDraft(view.config)
                      setDraftRevision(view.revision)
                    }}
                  >
                    Discard
                  </Button>
                )}
                <Button
                  disabled={busy || !valid}
                  onClick={() => setSection('simulation')}
                >
                  Test on history
                </Button>
                <Button
                  variant="primary"
                  busy={busy}
                  disabled={!editable || !changed || !valid}
                  onClick={() => void save()}
                >
                  {draft.mode === 'live'
                    ? 'Save & apply live'
                    : 'Save settings'}
                </Button>
              </div>
            </div>
          </div>
        </>
      )}
      {section === 'simulation' && !valid && (
        <Banner tone="warning">
          Correct your draft in Parameters before running a simulation.
        </Banner>
      )}
      <div className={section === 'simulation' ? 'contents' : 'hidden'}>
        <HistoricalSimulationForm
          name={name}
          config={draft}
          settlementDecimals={view.settlement_decimals}
          corridorDecimals={Number(view.dataset_template.corridor_decimals)}
          busy={busy || !valid}
          onStart={() => {
            setBusy(true)
            setError(null)
            setReport(null)
            setHistorical(null)
          }}
          onResult={(result) => {
            setHistorical(result)
            setReport(result.report)
          }}
          onError={setError}
          onDone={() => setBusy(false)}
        />
        <details className="text-sm">
          <summary className="mb-3 cursor-pointer font-bold">
            Advanced: import your own dataset
          </summary>
          <Card
            title="Import historical activity"
            action={
              <Button
                onClick={() =>
                  downloadJson(
                    'stitch-history-template.json',
                    view.dataset_template
                  )
                }
              >
                Download empty template
              </Button>
            }
          >
            <p className="text-muted mb-4 text-sm">
              Import observed prices and customer activity. Compare the same
              starting capital with and without your draft modules. Changing
              parameters does not place trades.
            </p>
            <Field label="Historical dataset (JSON)">
              <Input
                type="file"
                disabled={busy}
                accept=".json,application/json"
                onChange={(e) => void importFile(e.target.files?.[0])}
              />
            </Field>
            <p className="text-muted my-3 text-xs">
              {fileName || 'No dataset selected.'} Prices use settlement per
              corridor token; amounts are atomic-unit strings. Dealer depth is
              optional. See the dataset format below.
            </p>
            <Button
              variant="primary"
              disabled={!dataset || busy || !valid}
              busy={busy}
              onClick={() => void simulate()}
            >
              Run comparison
            </Button>
            <details className="mt-4 text-sm">
              <summary className="cursor-pointer font-bold">
                Dataset format
              </summary>
              <p className="my-2">
                Fill the template’s initial balances, vault caps, reserves and
                per-trade execution cost. Add chronological events with the
                following shape. Use Unix seconds and actual observed values.
              </p>
              <pre className="bg-canvas overflow-x-auto rounded-lg p-3 text-xs">
                {JSON.stringify(
                  {
                    at: 1700000000,
                    price_at: 1700000000,
                    price: 0.001,
                    trade: {
                      vault_buys: true,
                      corridor_amount: '1000000',
                      limit_price: 0.00099,
                    },
                    dealer: { max_corridor: '1000000', net_price: 0.000995 },
                  },
                  null,
                  2
                )}
              </pre>
              <p className="text-muted mt-2">
                Illustrative event only. Omit trade or dealer when absent. A
                customer limit is their minimum sale price when the vault buys,
                or maximum purchase price when the vault sells. Executed trades
                alone do not reveal those limits.
              </p>
            </details>
          </Card>
        </details>
        {historical && <HistoricalAnalysis result={historical} />}
        {report && (
          <>
            <p className="text-sm">
              Dynamic spreads in this run:{' '}
              {!report.config.spreads.enabled
                ? 'disabled'
                : report.config.spreads.inventory_aware &&
                    report.config.inventory.enabled
                  ? 'weighted by inventory'
                  : 'equal volatility additions'}
              .
            </p>
            <Banner tone="warning">
              Conditional simulation. Fixed customer activity and immediate
              settlement can overstate real returns.{' '}
              {report.dealer_observations === 0
                ? 'No dealer observations: no spot rebalance fills are simulated.'
                : historical?.options.dealer_scenario
                  ? `${report.dealer_observations.toLocaleString()} hypothetical dealer opportunities modeled.`
                  : `${report.dealer_observations.toLocaleString()} dealer observations supplied.`}
            </Banner>
            <Card
              title="Portfolio value over time"
              action={
                <Button
                  onClick={() =>
                    downloadJson(
                      'stitch-module-simulation.json',
                      historical ?? report
                    )
                  }
                >
                  Export results
                </Button>
              }
            >
              <svg
                viewBox="0 0 700 200"
                role="img"
                aria-label="Simulated baseline and module portfolio values over time"
                className="w-full"
              >
                <polyline
                  points={equityCoordinates(report.equity, 'baseline')}
                  fill="none"
                  stroke="currentColor"
                  opacity="0.4"
                  strokeWidth="2"
                />
                <polyline
                  points={equityCoordinates(report.equity, 'candidate')}
                  fill="none"
                  stroke="var(--tx-accent)"
                  strokeWidth="3"
                />
              </svg>
              <p className="text-muted text-xs">
                Gray: baseline · Purple: draft modules ·{' '}
                {clock(report.equity[0]?.at ?? 0)} –{' '}
                {clock(report.equity.at(-1)?.at ?? 0)}
              </p>
              <div className="mt-4 overflow-x-auto">
                <table className="w-full text-left text-sm">
                  <thead>
                    <tr>
                      <th className="py-2">Metric</th>
                      <th>Baseline</th>
                      <th>With modules</th>
                    </tr>
                  </thead>
                  <tbody>
                    {(
                      [
                        [
                          'Modeled return',
                          (m) =>
                            `${m.return_pct.toLocaleString(undefined, { maximumFractionDigits: 2 })}%`,
                        ],
                        [
                          'Ending value (settlement)',
                          (m) =>
                            formatAtomic(
                              m.ending_nav,
                              view.settlement_decimals
                            ),
                        ],
                        [
                          'Maximum drawdown',
                          (m) => percent(m.max_drawdown_bps),
                        ],
                        [
                          'Highest corridor share',
                          (m) => percent(m.max_inventory_bps),
                        ],
                        [
                          'Customer fills',
                          (m) => m.customer_fills.toLocaleString(),
                        ],
                        [
                          'Spot rebalance fills',
                          (m) => m.rebalance_fills.toLocaleString(),
                        ],
                        [
                          'Execution costs (settlement)',
                          (m) =>
                            formatAtomic(
                              m.execution_costs,
                              view.settlement_decimals
                            ),
                        ],
                      ] satisfies [
                        string,
                        (m: SimulationReport['baseline']) => string,
                      ][]
                    ).map(([label, render]) => (
                      <tr className="border-line-soft border-t" key={label}>
                        <td className="py-2">{label}</td>
                        <td>{render(report.baseline)}</td>
                        <td>{render(report.candidate)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              <p className="text-muted mt-3 text-xs">
                Results use the draft captured when this run started. Re-run
                after changing settings.
              </p>
            </Card>
            <Card title="Assumptions and limitations">
              <ul className="text-muted space-y-2 text-sm">
                {report.assumptions.map((a) => (
                  <li key={a}>{a}</li>
                ))}
              </ul>
            </Card>
          </>
        )}
      </div>
      {section === 'decisions' && (
        <Card
          title="Recent decisions"
          action={
            <Button
              disabled={!status}
              onClick={() =>
                downloadJson('stitch-module-decisions.json', status)
              }
            >
              Export
            </Button>
          }
        >
          <p className="text-muted mb-3 text-sm">
            Last 200 observations from this process. Times refer to
            observations; shadow decisions are proposals.
          </p>
          <Select
            aria-label="Filter decisions"
            value={decisionFilter}
            onChange={(e) => setDecisionFilter(e.target.value)}
          >
            <option value="all">All decisions</option>
            <option value="blocked">Blocked / warming up</option>
            <option value="rebalance">Spot sale proposed</option>
          </Select>
          {!status?.decisions.length && (
            <p className="text-muted py-6">No recorded decisions.</p>
          )}
          <div className="mt-3 space-y-2">
            {[...(status?.decisions ?? [])]
              .reverse()
              .filter(
                ({ decision: d }) =>
                  decisionFilter === 'all' ||
                  (decisionFilter === 'blocked'
                    ? d.blocked || isWarmingUp(d)
                    : d.rebalance_sell !== '0')
              )
              .map(({ decision: d }, i) => (
                <details
                  key={`${d.at}:${i}`}
                  className="border-line-soft rounded-lg border p-3 text-sm"
                >
                  <summary className="cursor-pointer">
                    {clock(d.at)} · {percent(d.inventory_bps)} corridor ·{' '}
                    {d.blocked
                      ? 'Blocked'
                      : `${spread(d.buy_bps)} / ${spread(d.sell_bps)}`}
                  </summary>
                  <ul className="text-muted mt-2 space-y-1">
                    {d.reasons.map((r, j) => (
                      <li key={j}>{r}</li>
                    ))}
                  </ul>
                </details>
              ))}
          </div>
        </Card>
      )}
    </section>
  )
}

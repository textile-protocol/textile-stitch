import { useEffect, useState } from 'react'
import { api } from '../api'
import { formatAtomic } from '../format'
import {
  downloadJson,
  equityCoordinates,
  moduleStatusFresh,
  type ModulesConfig,
  type ModulesView,
  type SimulationReport,
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
  Toggle,
} from './ui'

const percent = (bps: number | null) =>
  bps === null
    ? '—'
    : `${(bps / 100).toLocaleString(undefined, { maximumFractionDigits: 2 })}%`
const clock = (at: number) => new Date(at * 1000).toLocaleString()
const spread = (bps: number | null) =>
  bps === null ? 'Paused' : `${bps.toLocaleString()} bps`
const names = {
  inventory: 'Inventory balancing',
  spreads: 'Dynamic spreads',
  rebalance: 'Spot rebalancing',
}
type Section = 'overview' | 'parameters' | 'simulation' | 'decisions'

export default function ModulesPanel({
  name,
  editable,
}: {
  name: string
  editable: boolean
}) {
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
  const [decisionFilter, setDecisionFilter] = useState('all')

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
    void load()
    const interval = setInterval(() => void load(), 5000)
    return () => {
      cancelled = true
      clearInterval(interval)
    }
  }, [name])

  if (!view || !draft)
    return error ? <ErrorState error={error} /> : <Loading what="modules" />
  const changed = JSON.stringify(draft) !== JSON.stringify(view.config)
  const status = view.status
  const fresh = moduleStatusFresh(status, view.running, Date.now() / 1000)
  const latest = status?.decisions.at(-1)?.decision
  const activeConfig = status?.config

  function update<K extends keyof ModulesConfig>(
    key: K,
    value: ModulesConfig[K]
  ) {
    setDraft((current) => current && { ...current, [key]: value })
  }
  async function save() {
    if (!view || !draft) return
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
    if (!draft || !dataset) return
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
  const numeric = (
    label: string,
    value: number,
    onChange: (v: number) => void,
    hint?: string
  ) => (
    <Field label={label} hint={hint}>
      <Input
        type="number"
        min="0"
        step="any"
        value={value}
        onChange={(e) => onChange(Number(e.target.value))}
      />
    </Field>
  )

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-xl font-bold">FX protection modules</h2>
          <p className="text-muted text-sm">
            Textile strategies for this vault’s inventory and quotes.
          </p>
        </div>
        <span className="bg-hover rounded-full px-3 py-1 text-sm">
          {fresh && activeConfig
            ? `${activeConfig.mode} · observing`
            : view.running
              ? 'Waiting for current telemetry'
              : 'Bot stopped'}
        </span>
      </div>
      <div className="flex flex-wrap gap-2" aria-label="Module sections">
        {(['overview', 'parameters', 'simulation', 'decisions'] as const).map(
          (s) => (
            <Button
              key={s}
              variant={section === s ? 'primary' : 'ghost'}
              onClick={() => setSection(s)}
            >
              {s === 'simulation'
                ? 'Historical simulation'
                : s.charAt(0).toUpperCase() + s.slice(1)}
            </Button>
          )
        )}
      </div>
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
        <Banner tone="warning">
          Unsaved parameters. Simulations use this draft; the running bot keeps
          its current configuration.
        </Banner>
      )}
      {section === 'overview' && (
        <>
          <div className="grid gap-4 sm:grid-cols-3">
            <Card title="Corridor inventory">
              <p className="text-3xl font-bold">
                {fresh && latest ? percent(latest.inventory_bps) : '—'}
              </p>
              <p className="text-muted mt-2 text-xs">
                Share of free vault value, including settlement deployed for
                yield.
              </p>
            </Card>
            <Card title="Proposed buy / sell spread">
              <p className="text-xl font-bold">
                {fresh && latest
                  ? `${spread(latest.buy_bps)} / ${spread(latest.sell_bps)}`
                  : '—'}
              </p>
              <p className="text-muted mt-2 text-xs">
                Shadow proposals do not change live quotes.
              </p>
            </Card>
            <Card title="Spot rebalancing">
              <p className="text-sm">
                {fresh ? status?.rebalance_status : 'No current status'}
              </p>
              <p className="text-muted mt-2 text-xs">
                Dealer acceptance is not a confirmed fill.
              </p>
            </Card>
          </div>
          <div className="grid gap-4 sm:grid-cols-3">
            {(['inventory', 'spreads', 'rebalance'] as const).map((key) => (
              <Card key={key} title={names[key]}>
                <p className="text-muted mb-3 text-sm">
                  {key === 'inventory'
                    ? 'Encourage trades that reduce excess corridor currency. Stop accumulating at the limit.'
                    : key === 'spreads'
                      ? 'Widen spreads when recent market prices move. Wait for enough fresh history.'
                      : 'Sell excess corridor currency for settlement through a configured dealer.'}
                </p>
                <p className="text-sm font-bold">
                  Saved setting:{' '}
                  {view.config[key].enabled ? view.config.mode : 'off'}
                </p>
                <Button
                  className="mt-3"
                  onClick={() => setSection('parameters')}
                >
                  Configure
                </Button>
              </Card>
            ))}
          </div>
          {latest && fresh && (
            <Card title="Latest decision">
              <ul className="space-y-1 text-sm">
                {latest.reasons.map((r, i) => (
                  <li key={i}>{r}</li>
                ))}
              </ul>
            </Card>
          )}
          {!status?.decisions.length && (
            <Banner tone="info">
              No decisions yet. Start the bot with modules enabled to collect
              observations. Imported historical data can be simulated while the
              bot is stopped.
            </Banner>
          )}
        </>
      )}
      {section === 'parameters' && (
        <>
          <Card title="Operation mode">
            <Field
              label="Mode"
              hint="Shadow records proposals. Live applies enabled strategies. Off stops new module activity; signed orders remain valid until expiry."
            >
              <Select
                value={draft.mode}
                onChange={(e) =>
                  update('mode', e.target.value as ModulesConfig['mode'])
                }
              >
                <option value="off">Off</option>
                <option value="shadow">Shadow</option>
                <option value="live">Live</option>
              </Select>
            </Field>
          </Card>
          <div className="grid gap-4 lg:grid-cols-3">
            <Card title="Inventory balancing">
              <div className="space-y-3">
                <Toggle
                  label="Enable inventory balancing"
                  checked={draft.inventory.enabled}
                  onChange={(enabled) =>
                    update('inventory', { ...draft.inventory, enabled })
                  }
                />
                {numeric(
                  'Target share (bps)',
                  draft.inventory.target_bps,
                  (target_bps) =>
                    update('inventory', { ...draft.inventory, target_bps }),
                  '100 bps = 1% of vault value.'
                )}
                {numeric(
                  'Maximum share (bps)',
                  draft.inventory.max_bps,
                  (max_bps) =>
                    update('inventory', { ...draft.inventory, max_bps })
                )}
                {numeric(
                  'Maximum price skew (bps)',
                  draft.inventory.max_skew_bps,
                  (max_skew_bps) =>
                    update('inventory', { ...draft.inventory, max_skew_bps })
                )}
                {numeric(
                  'Minimum spread (bps)',
                  draft.inventory.spread_floor_bps,
                  (spread_floor_bps) =>
                    update('inventory', {
                      ...draft.inventory,
                      spread_floor_bps,
                    })
                )}
              </div>
            </Card>
            <Card title="Dynamic spreads">
              <div className="space-y-3">
                <Toggle
                  label="Enable dynamic spreads"
                  checked={draft.spreads.enabled}
                  onChange={(enabled) =>
                    update('spreads', { ...draft.spreads, enabled })
                  }
                />
                {numeric(
                  'Price window (seconds)',
                  draft.spreads.window_secs,
                  (window_secs) =>
                    update('spreads', { ...draft.spreads, window_secs })
                )}
                {numeric(
                  'Warmup (seconds)',
                  draft.spreads.warmup_secs,
                  (warmup_secs) =>
                    update('spreads', { ...draft.spreads, warmup_secs })
                )}
                {numeric(
                  'Movement multiplier',
                  draft.spreads.multiplier,
                  (multiplier) =>
                    update('spreads', { ...draft.spreads, multiplier })
                )}
                {numeric(
                  'Maximum extra spread (bps)',
                  draft.spreads.max_extra_bps,
                  (max_extra_bps) =>
                    update('spreads', { ...draft.spreads, max_extra_bps })
                )}
              </div>
            </Card>
            <Card title="Spot rebalancing">
              <div className="space-y-3">
                <Toggle
                  label="Enable spot rebalancing"
                  checked={draft.rebalance.enabled}
                  onChange={(enabled) =>
                    update('rebalance', { ...draft.rebalance, enabled })
                  }
                />
                {numeric(
                  'Trigger share (bps)',
                  draft.rebalance.trigger_bps,
                  (trigger_bps) =>
                    update('rebalance', { ...draft.rebalance, trigger_bps })
                )}
                {numeric(
                  'Maximum sale / NAV (bps)',
                  draft.rebalance.max_trade_bps,
                  (max_trade_bps) =>
                    update('rebalance', { ...draft.rebalance, max_trade_bps })
                )}
                {numeric(
                  'Maximum slippage (bps)',
                  draft.rebalance.max_slippage_bps,
                  (max_slippage_bps) =>
                    update('rebalance', {
                      ...draft.rebalance,
                      max_slippage_bps,
                    })
                )}
                {numeric(
                  'Cooldown (seconds)',
                  draft.rebalance.cooldown_secs,
                  (cooldown_secs) =>
                    update('rebalance', { ...draft.rebalance, cooldown_secs })
                )}
                {numeric(
                  'Order lifetime (seconds)',
                  draft.rebalance.order_lifetime_secs,
                  (order_lifetime_secs) =>
                    update('rebalance', {
                      ...draft.rebalance,
                      order_lifetime_secs,
                    })
                )}
              </div>
            </Card>
          </div>
          <Card title="Rebalance dealer">
            <div className="space-y-3">
              <Toggle
                label="Configure a dealer"
                checked={!!draft.rebalance.dealer}
                onChange={(enabled) =>
                  update('rebalance', {
                    ...draft.rebalance,
                    dealer: enabled
                      ? { url: '', taker: '', api_key_env: null }
                      : null,
                  })
                }
              />
              {draft.rebalance.dealer ? (
                <div className="grid gap-3 sm:grid-cols-3">
                  <Field label="Dealer URL">
                    <Input
                      value={draft.rebalance.dealer.url}
                      onChange={(e) =>
                        update('rebalance', {
                          ...draft.rebalance,
                          dealer: {
                            ...draft.rebalance.dealer!,
                            url: e.target.value,
                          },
                        })
                      }
                    />
                  </Field>
                  <Field label="Counterparty wallet">
                    <Input
                      value={draft.rebalance.dealer.taker}
                      onChange={(e) =>
                        update('rebalance', {
                          ...draft.rebalance,
                          dealer: {
                            ...draft.rebalance.dealer!,
                            taker: e.target.value,
                          },
                        })
                      }
                    />
                  </Field>
                  <Field
                    label="Credential environment variable"
                    hint="Variable name only. Keep the credential in the bot’s environment."
                  >
                    <Input
                      value={draft.rebalance.dealer.api_key_env ?? ''}
                      onChange={(e) =>
                        update('rebalance', {
                          ...draft.rebalance,
                          dealer: {
                            ...draft.rebalance.dealer!,
                            api_key_env: e.target.value || null,
                          },
                        })
                      }
                    />
                  </Field>
                </div>
              ) : (
                <p className="text-muted text-sm">
                  Rebalancing can propose sales and run simulations. Live sales
                  require a dealer that supports the Stitch rebalance API and
                  obtains Warp’s co-signature.
                </p>
              )}
            </div>
          </Card>
          <div className="flex flex-wrap gap-2">
            <Button
              variant="primary"
              busy={busy}
              disabled={!editable || !changed}
              onClick={() => void save()}
            >
              {draft.mode === 'live'
                ? 'Save and apply live modules'
                : 'Save module settings'}
            </Button>
            <Button
              onClick={() => {
                setDraft(view.config)
                setDraftRevision(view.revision)
              }}
            >
              Discard draft
            </Button>
            <Button onClick={() => setSection('simulation')}>
              Test draft first
            </Button>
          </div>
          <p className="text-muted text-xs">
            Saving restarts a running bot. A stopped bot stays stopped.
          </p>
        </>
      )}
      {section === 'simulation' && (
        <>
          <Card
            title="Test historical activity"
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
              disabled={!dataset}
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
          {report && (
            <>
              <Banner tone="warning">
                Conditional simulation. Fixed customer activity and immediate
                settlement can overstate real returns.{' '}
                {report.dealer_observations === 0
                  ? 'No dealer observations: no spot rebalance fills are simulated.'
                  : `${report.dealer_observations.toLocaleString()} dealer observations supplied.`}
              </Banner>
              <Card
                title="Portfolio value over time"
                action={
                  <Button
                    onClick={() =>
                      downloadJson('stitch-module-simulation.json', report)
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
                            'Net return',
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
        </>
      )}
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
                    ? d.blocked
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
    </div>
  )
}

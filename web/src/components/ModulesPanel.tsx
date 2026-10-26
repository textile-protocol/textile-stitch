import { CurveQuotes } from './CurveModule'
import ManualSales from './ManualSales'
import { useEffect, useState } from 'react'
import { api } from '../api'
import { poll } from '../poll'
import ModulesOverview from './ModulesOverview'
import ModuleParameters from './ModuleParameters'
import ModuleSaveBar from './ModuleSaveBar'
import { pairSymbols } from './SpreadExample'
import ScrollTabs from './ScrollTabs'
import {
  corridorSpreads,
  liveReading,
  moduleChanges,
  moduleParameterErrors,
  modulesFor,
  type ModuleKey,
} from '../modulePresentation'
import {
  downloadJson,
  isWarmingUp,
  type ModulesConfig,
  type ModulesView,
} from '../modules'
import { Banner, Button, Card, ErrorState, Loading, Select } from './ui'

const percent = (bps: number | null) =>
  bps === null
    ? '—'
    : `${(bps / 100).toLocaleString(undefined, { maximumFractionDigits: 2 })}%`
const clock = (at: number) => new Date(at * 1000).toLocaleString()
const spread = (bps: number | null) =>
  bps === null ? 'Paused' : `${bps.toLocaleString()} bps`
type Section = 'sales' | 'overview' | 'parameters' | 'decisions'

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
  const [decisionFilter, setDecisionFilter] = useState('all')
  const [selectedModule, setSelectedModule] = useState<ModuleKey | null>(
    'inventory'
  )

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
  // Older panels only served vault bots.
  const vault = view.vault ?? true
  const changed = JSON.stringify(draft) !== JSON.stringify(view.config)
  const status = view.status
  const parameterErrors = moduleParameterErrors(draft, vault)
  // The editor opens on inventory balancing; a wallet bot has only the curve.
  const available = modulesFor(vault)
  const openModule =
    selectedModule && !available.includes(selectedModule)
      ? available[0]!
      : selectedModule
  const valid = parameterErrors.length === 0
  // Corridor names use " / ", "→" or "↔" depending on where they were listed.
  const symbols = pairSymbols(pair)
  const currency = symbols?.base ?? 'Corridor currency'
  const settlement = symbols?.quote ?? 'Settlement currency'
  // An absolute corridor offset reads as bps at the bot's last price.
  const corridor = corridorSpreads(
    view.corridor_spreads,
    status?.decisions.at(-1)?.price
  )

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
  return (
    <section aria-label="Module workspace" className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-xl font-bold">Modules</h2>
          <p className="text-muted text-sm">
            {vault
              ? 'Manage your holdings and quote margins.'
              : 'Lean your Swap quotes on what this wallet holds.'}
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
          // Sales move a vault's corridor currency; a wallet has none to run.
          ...(vault ? [{ value: 'sales' as const, label: 'Manual sale' }] : []),
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
      {changed && section !== 'parameters' && (
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
          settlement={settlement}
          onConfigure={(key) => {
            if (key === 'rebalance' && vault) {
              setSection('sales')
              return
            }
            setSelectedModule(key)
            setSection('parameters')
          }}
          onDecisions={() => setSection('decisions')}
        />
      )}
      {section === 'parameters' && (
        <>
          <fieldset disabled={busy} className="min-w-0">
            <ModuleParameters
              draft={draft}
              onChange={setDraft}
              selected={openModule}
              onSelect={setSelectedModule}
              currency={currency}
              settlement={settlement}
              corridor={corridor}
              live={liveReading(view, now)}
              liquidityRefusal={view.liquidity_refusal ?? null}
              view={view}
              now={now}
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
          <ModuleSaveBar
            dirty={changed}
            changes={changed ? moduleChanges(view.config, draft) : []}
            live={draft.mode === 'live'}
            valid={valid}
            editable={editable}
            busy={busy}
            onDiscard={() => {
              setDraft(view.config)
              setDraftRevision(view.revision)
            }}
            onSave={() => void save()}
          />
        </>
      )}
      {section === 'decisions' && (draft.curve.enabled || status?.curve) && (
        <CurveQuotes
          quotes={status?.curve?.quotes ?? []}
          currency={currency}
          settlement={settlement}
        />
      )}
      {section === 'decisions' && vault && (
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

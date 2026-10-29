import { CurveQuotes } from './CurveModule'
import { UnwindQuotes } from './ProfitUnwindModule'
import ManualSales from './ManualSales'
import { useEffect, useState } from 'react'
import type { QuoteModulesState } from '../useQuoteModules'
import { quoteConfigForScope, type ModuleSection } from '../moduleWorkspace'
import ModulesOverview from './ModulesOverview'
import ModuleParameters from './ModuleParameters'
import ModuleSaveBar from './ModuleSaveBar'
import { pairSymbols } from './SpreadExample'
import {
  corridorSpreads,
  enforcedUnwindCapLimit,
  liveReading,
  moduleChanges,
  moduleParameterErrors,
  type ModuleKey,
} from '../modulePresentation'
import { downloadJson, isWarmingUp } from '../modules'
import { Banner, Button, Card, ErrorState, Loading, Select } from './ui'

const percent = (bps: number | null) =>
  bps === null
    ? '—'
    : `${(bps / 100).toLocaleString(undefined, { maximumFractionDigits: 2 })}%`
const clock = (at: number) => new Date(at * 1000).toLocaleString()
const spread = (bps: number | null) =>
  bps === null ? 'Paused' : `${bps.toLocaleString()} bps`

export default function ModulesPanel({
  name,
  editable,
  pair,
  state,
  selectedModule,
  section,
  onSection,
}: {
  name: string
  editable: boolean
  pair?: string
  state: QuoteModulesState
  selectedModule: ModuleKey
  section: ModuleSection
  onSection: (section: ModuleSection) => void
}) {
  const [now, setNow] = useState(() => Date.now() / 1000)
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now() / 1000), 1000)
    return () => clearInterval(timer)
  }, [])
  const { view, draft, setDraft, base, configFor, error, busy, save, discard } =
    state
  const [decisionFilter, setDecisionFilter] = useState('all')

  const scoped = configFor(selectedModule)
  if (!view || !draft || !base || !scoped)
    return error ? <ErrorState error={error} /> : <Loading what="modules" />
  // Older panels only served vault bots.
  const changed = JSON.stringify(scoped) !== JSON.stringify(base.config)
  const vault = view.vault ?? true
  const status = view.status
  const parameterErrors = moduleParameterErrors(
    scoped,
    vault,
    enforcedUnwindCapLimit(view, base.config, scoped, now)
  )
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

  return (
    <section aria-label="Quote module workspace" className="space-y-4">
      {changed && section !== 'settings' && (
        <Banner tone="info">
          You have unsaved changes. Live settings stay unchanged until you save.
        </Banner>
      )}
      {section === 'overview' && selectedModule === 'rebalance' && vault && (
        <ManualSales
          name={name}
          view={view}
          editable={editable}
          currency={currency}
          settlement={settlement}
          onConfigure={() => onSection('settings')}
        />
      )}
      {section === 'overview' && selectedModule !== 'rebalance' && (
        <ModulesOverview
          selected={selectedModule}
          view={view}
          now={now}
          currency={currency}
          settlement={settlement}
          onDecisions={() => onSection('activity')}
        />
      )}
      {section === 'settings' && (
        <>
          <fieldset disabled={busy} className="min-w-0">
            <ModuleParameters
              draft={scoped}
              onChange={(next) =>
                setDraft(quoteConfigForScope(draft, next, selectedModule))
              }
              selected={selectedModule}
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
            changes={
              changed
                ? moduleChanges(
                    base.config,
                    scoped,
                    `${currency} per ${settlement}`
                  )
                : []
            }
            live={scoped.mode === 'live'}
            valid={valid}
            editable={editable}
            busy={busy}
            onDiscard={() => discard(selectedModule)}
            onSave={() => void save(selectedModule)}
          />
        </>
      )}
      {section === 'activity' && selectedModule === 'curve' && (
        <CurveQuotes
          quotes={status?.curve?.quotes ?? []}
          currency={currency}
          settlement={settlement}
        />
      )}
      {section === 'activity' && selectedModule === 'profit_unwind' && (
        <UnwindQuotes
          quotes={status?.profit_unwind?.quotes ?? []}
          currency={currency}
          settlement={settlement}
        />
      )}
      {section === 'activity' &&
        vault &&
        selectedModule !== 'curve' &&
        selectedModule !== 'profit_unwind' && (
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

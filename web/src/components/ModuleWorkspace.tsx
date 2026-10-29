import { useEffect, useState } from 'react'
import { useSearchParams } from 'react-router-dom'
import {
  MODULE_CATEGORIES,
  WORKSPACE_MODULES,
  listedModules,
  quoteModuleStatus,
  workspaceModules,
  selectedWorkspaceModule,
  type BotModuleId,
  type ModuleSection,
  type WorkspaceModuleId,
} from '../moduleWorkspace'
import {
  leanConflict,
  moduleModes,
  moduleKeyFigures,
} from '../modulePresentation'
import { hedgeStatus } from '../perps'
import type { Bot, Funding } from '../types'
import type { PerpsState } from '../usePerps'
import { useQuoteModules } from '../useQuoteModules'
import ModulesPanel from './ModulesPanel'
import PerpsModule from './PerpsModule'
import ScrollTabs from './ScrollTabs'
import { Banner, Button, Input, Loading, Select } from './ui'

/** Navigation is presentation only. Both editors stay mounted so their drafts survive. */
export default function ModuleWorkspace({
  ids,
  bot,
  perps,
  funding,
}: {
  ids: readonly BotModuleId[]
  bot: Bot
  perps: PerpsState
  funding: Funding | null
}) {
  const quotes = useQuoteModules(bot.name, ids.includes('quotes'), bot.editable)
  const [params, setParams] = useSearchParams()
  const [filter, setFilter] = useState('')
  const [library, setLibrary] = useState(false)
  const [hedgeDirty, setHedgeDirty] = useState(false)
  const [now, setNow] = useState(Date.now)
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(timer)
  }, [])
  const available = workspaceModules(ids, quotes.view)
  const requested = params.get('module')
  const selected = selectedWorkspaceModule(available, requested, quotes.view)
  const rawSection = params.get('section')
  const section: ModuleSection =
    rawSection === 'settings' || rawSection === 'activity'
      ? rawSection
      : 'overview'
  const listed = listedModules(available, selected, quotes.view, quotes.draft)
  const multiple = listed.length > 1
  const quoteModule =
    available.find((id) => id !== 'hedge' && id === selected) ??
    available.find((id) => id !== 'hedge')

  function navigate(id: WorkspaceModuleId, next: ModuleSection = 'overview') {
    setParams(
      (previous) => {
        const nextParams = new URLSearchParams(previous)
        nextParams.set('module', id)
        nextParams.set('section', next)
        return nextParams
      },
      { replace: true }
    )
    setLibrary(false)
  }
  const status = (id: WorkspaceModuleId) =>
    id === 'hedge'
      ? hedgeStatus(perps.view, now, Boolean(perps.error))
      : quotes.error
        ? 'Unavailable'
        : quotes.view
          ? quoteModuleStatus(quotes.view, id, now / 1000)
          : 'Loading'
  const unsaved = (id: WorkspaceModuleId) =>
    id === 'hedge'
      ? hedgeDirty
      : quotes.view &&
        quotes.draft &&
        (JSON.stringify(quotes.view.config[id]) !==
          JSON.stringify(quotes.draft[id]) ||
          quotes.view.config.mode !== quotes.draft.mode)
  const groups = MODULE_CATEGORIES.map((category) => ({
    category,
    entries: listed.filter(
      (id) =>
        WORKSPACE_MODULES[id].category === category &&
        `${WORKSPACE_MODULES[id].name} ${category}`
          .toLowerCase()
          .includes(filter.trim().toLowerCase())
    ),
  }))

  return (
    <section aria-label="Bot module workspace" className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-xl font-bold">Modules</h2>
          <p className="text-muted mt-1 text-sm">
            Monitor and configure how this bot operates.
          </p>
        </div>
        {available.length > 0 && (
          <Button
            aria-expanded={library}
            aria-controls="module-library"
            onClick={() => setLibrary(!library)}
          >
            {library ? 'Close module list' : 'Manage modules'}
          </Button>
        )}
      </div>
      {library && (
        <div
          id="module-library"
          className="border-line-soft bg-surface rounded-xl border p-4"
        >
          <p className="text-muted mb-3 text-sm">
            Available for this bot. Open a module to review its settings;
            opening it does not enable it.
          </p>
          <div className="grid gap-2 sm:grid-cols-2">
            {available.map((id) => {
              const conflict =
                id !== 'hedge' && quotes.draft && quotes.view
                  ? leanConflict(quotes.draft, id, quotes.view.vault ?? true)
                  : null
              const reason =
                conflict ||
                (id === 'liquidity' ? quotes.view?.liquidity_refusal : null)
              return (
                <button
                  key={id}
                  onClick={() => navigate(id, 'settings')}
                  className="border-line-soft hover:bg-hover rounded-lg border p-3 text-left"
                >
                  <span className="block text-sm font-bold">
                    {WORKSPACE_MODULES[id].name}
                  </span>
                  <span className="text-muted mt-1 block text-xs">
                    {reason || WORKSPACE_MODULES[id].description}
                  </span>
                  {id !== 'hedge' && quotes.view && (
                    <span className="text-muted mt-2 block text-xs tabular-nums">
                      {moduleKeyFigures(quotes.view.config, id)
                        .map((figure) => `${figure.label} ${figure.value}`)
                        .join(' · ')}
                    </span>
                  )}
                </button>
              )
            })}
          </div>
        </div>
      )}
      {/* Errors stay independent: a missing quote service never hides hedge controls. */}
      {ids.includes('quotes') &&
        !quotes.view &&
        (quotes.error ? (
          <Banner tone="danger">
            Quote modules unavailable: {quotes.error}
          </Banner>
        ) : (
          <Loading what="quote modules" />
        ))}
      {selected && (
        <div
          className={`border-line-soft bg-surface grid rounded-xl border ${multiple ? 'md:grid-cols-[13rem_minmax(0,1fr)]' : ''}`}
        >
          {multiple && (
            <>
              <div className="border-line-soft border-b p-4 md:hidden">
                <label
                  className="text-muted mb-2 block text-xs font-bold"
                  htmlFor="module-select"
                >
                  Module
                </label>
                <Select
                  id="module-select"
                  aria-label="Select module"
                  value={selected}
                  onChange={(e) =>
                    navigate(e.target.value as WorkspaceModuleId)
                  }
                >
                  {listed.map((id) => (
                    <option key={id} value={id}>
                      {WORKSPACE_MODULES[id].name} · {status(id)}
                      {unsaved(id) ? ' · Unsaved' : ''}
                    </option>
                  ))}
                </Select>
              </div>
              <nav
                aria-label="Modules"
                className="border-line-soft hidden min-w-0 border-r p-3 md:block"
              >
                <Input
                  aria-label="Search modules"
                  placeholder="Search modules…"
                  value={filter}
                  onChange={(e) => setFilter(e.target.value)}
                />
                {groups.map(
                  ({ category, entries }) =>
                    entries.length > 0 && (
                      <div key={category} className="mt-5">
                        <p className="text-faint mb-2 px-2 text-[10px] font-bold uppercase tracking-widest">
                          {category}
                        </p>
                        <div className="space-y-1">
                          {entries.map((id) => (
                            <button
                              key={id}
                              aria-current={
                                selected === id ? 'page' : undefined
                              }
                              onClick={() => navigate(id)}
                              className={`w-full rounded-lg px-3 py-3 text-left transition ${selected === id ? 'bg-accent-tint text-accent' : 'hover:bg-hover'}`}
                            >
                              <span className="block text-sm font-bold">
                                {WORKSPACE_MODULES[id].name}
                              </span>
                              <span className="mt-1.5 flex items-center gap-1.5 text-xs">
                                <ModuleStatus value={status(id)} />
                                {unsaved(id) && (
                                  <span className="text-warning">
                                    · Unsaved
                                  </span>
                                )}
                              </span>
                            </button>
                          ))}
                        </div>
                      </div>
                    )
                )}
                {groups.every((group) => !group.entries.length) && (
                  <p className="text-muted p-3 text-xs">No matching modules.</p>
                )}
                <p className="text-faint mt-8 px-2 text-xs leading-relaxed">
                  Each module keeps its settings. Quote modules share one policy
                  mode.
                </p>
              </nav>
            </>
          )}
          <div className="min-w-0 p-4 sm:p-6">
            {ids.includes('hedge') && (
              <div hidden={selected !== 'hedge'}>
                <PerpsModule
                  bot={bot}
                  perps={perps}
                  funding={funding}
                  section={section}
                  onDirtyChange={setHedgeDirty}
                  onSection={(next) => navigate('hedge', next)}
                />
              </div>
            )}
            {quoteModule && quoteModule !== 'hedge' && (
              <div hidden={selected === 'hedge'} className="space-y-5">
                <header className="flex flex-wrap items-start justify-between gap-3">
                  <div>
                    <h3 className="text-xl font-bold">
                      {WORKSPACE_MODULES[quoteModule].name}
                    </h3>
                    <p className="text-muted mt-2 text-sm">
                      {WORKSPACE_MODULES[quoteModule].description}
                    </p>
                    <p className="text-muted mt-2 text-xs">
                      Saved quote policy:{' '}
                      {
                        moduleModes.find(
                          (m) => m.value === quotes.view?.config.mode
                        )?.label
                      }
                      . Shared by enabled quote modules.
                    </p>
                  </div>
                  <ModuleStatus value={status(quoteModule)} />
                </header>
                <ScrollTabs
                  label="Module sections"
                  value={section}
                  onChange={(next) => navigate(quoteModule, next)}
                  items={[
                    { value: 'overview', label: 'Overview' },
                    { value: 'settings', label: 'Settings' },
                    { value: 'activity', label: 'Activity' },
                  ]}
                />
                <ModulesPanel
                  name={bot.name}
                  editable={bot.editable}
                  pair={bot.config?.pairs[0]}
                  state={quotes}
                  selectedModule={quoteModule}
                  section={section}
                  onSection={(next) => navigate(quoteModule, next)}
                />
              </div>
            )}
          </div>
        </div>
      )}
    </section>
  )
}

function ModuleStatus({ value }: { value: string }) {
  const tone =
    value === 'Live'
      ? 'text-success'
      : [
            'Paused',
            'Stale',
            'Unavailable',
            'Blocked',
            'Update pending',
          ].includes(value)
        ? 'text-warning'
        : 'text-muted'
  return (
    <span
      className={`inline-flex items-center gap-1.5 text-xs font-bold ${tone}`}
    >
      <span aria-hidden className="size-1.5 rounded-full bg-current" />
      {value}
    </span>
  )
}

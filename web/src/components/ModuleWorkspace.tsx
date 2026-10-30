import { useEffect, useState } from 'react'
import { useSearchParams } from 'react-router-dom'
import {
  MODULE_CATEGORIES,
  WORKSPACE_MODULES,
  quoteModuleStatus,
  quoteModuleUnsaved,
  workspaceModules,
  selectedWorkspaceModule,
  type BotModuleId,
  hedgeBadge,
  type BadgeMode,
  type ModuleSection,
  type WorkspaceModuleId,
} from '../moduleWorkspace'
import { modeLabel } from '../modulePresentation'
import { moduleMode } from '../moduleModes'
import { hedgeStatus } from '../perps'
import type { Bot, Funding } from '../types'
import type { PerpsState } from '../usePerps'
import { useQuoteModules } from '../useQuoteModules'
import ModulesPanel from './ModulesPanel'
import PerpsModule from './PerpsModule'
import ScrollTabs from './ScrollTabs'
import { Banner, Input, Loading } from './ui'

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
        quotes.base &&
        quotes.draft &&
        quoteModuleUnsaved(
          { ...quotes.view, config: quotes.base.config },
          quotes.draft,
          id
        )
  const groups = MODULE_CATEGORIES.map((category) => ({
    category,
    entries: available.filter(
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
      </div>
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
      {quotes.view && quotes.error && (
        <Banner tone="danger" onDismiss={() => quotes.setError(null)}>
          {quotes.error}
        </Banner>
      )}
      {quotes.note && (
        <Banner tone="info" onDismiss={() => quotes.setNote(null)}>
          {quotes.note}
        </Banner>
      )}
      {selected && (
        <div className="border-line-soft bg-surface grid rounded-xl border md:grid-cols-[14rem_minmax(0,1fr)]">
          <nav
            aria-label="Modules"
            className="border-line-soft min-w-0 border-b p-3 md:border-r md:border-b-0"
          >
            <p className="px-2 pt-1 text-sm font-bold">All modules</p>
            <p className="text-muted px-2 pt-1 pb-3 text-xs">
              Saved mode · current activity
            </p>
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
                    <div className="grid grid-cols-2 gap-1 md:grid-cols-1">
                      {entries.map((id) => {
                        const mode =
                          id === 'hedge'
                            ? hedgeBadge(perps.view)
                            : quotes.view
                              ? moduleMode(quotes.view.config, id)
                              : null
                        // Activity beyond what the mode badge already says.
                        const activity =
                          mode && mode !== 'off' ? status(id) : null
                        return (
                          <button
                            key={id}
                            aria-current={selected === id ? 'page' : undefined}
                            onClick={() => navigate(id)}
                            className={`min-w-0 w-full rounded-lg px-3 py-3 text-left transition ${selected === id ? 'bg-accent-tint text-accent' : 'hover:bg-hover'}`}
                          >
                            <span className="block text-sm font-bold">
                              {WORKSPACE_MODULES[id].name}
                            </span>
                            <span className="mt-2 flex flex-wrap items-center gap-1.5 text-xs">
                              <span
                                className={`rounded px-1.5 py-0.5 font-bold ${mode ? badgeTone[mode] : 'bg-canvas text-muted'}`}
                              >
                                {mode === null
                                  ? 'Loading'
                                  : mode === 'on'
                                    ? 'On'
                                    : modeLabel(mode)}
                              </span>
                              {activity &&
                                !['Live', 'Preview'].includes(activity) && (
                                  <ModuleStatus value={activity} />
                                )}
                              {unsaved(id) && (
                                <span className="text-warning">Unsaved</span>
                              )}
                            </span>
                          </button>
                        )
                      })}
                    </div>
                  </div>
                )
            )}
            {groups.every((group) => !group.entries.length) && (
              <p className="text-muted p-3 text-xs">No matching modules.</p>
            )}
          </nav>
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

const badgeTone: Record<BadgeMode, string> = {
  live: 'bg-success/10 text-success',
  on: 'bg-success/10 text-success',
  shadow: 'bg-warning/10 text-warning',
  off: 'bg-canvas text-muted',
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

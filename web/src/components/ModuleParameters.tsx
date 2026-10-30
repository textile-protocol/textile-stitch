import { useId, useState } from 'react'
import type { ModulesConfig, ModulesView } from '../modules'
import { CurveExample, CurveFields } from './CurveModule'
import { ProfitUnwindFields, UnwindExample } from './ProfitUnwindModule'
import ModulePresets from './ModulePresets'
import {
  leanConflict,
  liquidityScheduleErrors,
  moduleNames,
  moduleParameterErrors,
  percentage,
  liveRow,
  quoteBreakdown,
  quoteModeNote,
  warmupNote,
  type LiveReading,
  type ModuleKey,
  type QuoteSides,
} from '../modulePresentation'
import {
  AllocationBar,
  Chevron,
  ModuleSwitch,
  NumberControl,
  Popover,
  QuoteBreakdown,
  ScheduleStrip,
} from './ModuleControls'
import {
  InventoryFields,
  LiquidityFields,
  moduleGuides,
  RebalanceFields,
  SpreadFields,
} from './ModuleParameterFields'

type Example = {
  share: number
  /** Price range in bps. Fractional: a live reading keeps the bot's own float. */
  range: number
  /** The inventory curve's example trade, in settlement units. */
  sizeUsd: number
  setShare: (bps: number) => void
  setRange: (bps: number) => void
  setSizeUsd: (usd: number) => void
  /** The bot's current reading, when it has a fresh one. */
  live: LiveReading | null
  /** Holdings and range are the live reading's, so the table is "now". */
  atLive: boolean
  useLive: () => void
  /** Collapsed to a narrow tab by default; shared by all modules. */
  shown: boolean
  toggle: () => void
}

/** One module editor; the shared quote policy remains explicit and revision-checked. */
export default function ModuleParameters({
  draft,
  onChange,
  selected,
  currency,
  settlement,
  corridor,
  live,
  liquidityRefusal,
  view,
  now,
}: {
  draft: ModulesConfig
  onChange: (config: ModulesConfig) => void
  selected: ModuleKey
  currency: string
  settlement: string
  /** The corridor's saved spreads in bps; null until they can be read. */
  corridor: QuoteSides | null
  /** The bot's current reading. The example starts from it when there is one. */
  live: LiveReading | null
  /** Why the liquidity schedule can't run on this vault, or null. */
  liquidityRefusal: string | null
  /** The module view: the inventory curve reads its status and checks. */
  view: ModulesView
  now: number
}) {
  const vault = view.vault ?? true
  // A warming-up window has no range to start from; 0.2% is a plain example.
  const heldShare =
    view.status?.curve?.reading?.share ??
    view.status?.profit_unwind?.reading?.share
  const [share, setShare] = useState(
    live?.share ?? (heldShare != null ? Math.round(heldShare * 10_000) : 4500)
  )
  const [range, setRange] = useState(live?.rangeBps ?? 20)
  const [sizeUsd, setSizeUsd] = useState(10_000)
  const [shown, setShown] = useState(false)
  const example: Example = {
    share,
    range,
    sizeUsd,
    setSizeUsd: (value) =>
      setSizeUsd(Number.isFinite(value) ? Math.max(0, value) : 0),
    shown,
    live,
    atLive:
      live !== null &&
      live.rangeBps !== null &&
      share === live.share &&
      range === live.rangeBps,
    useLive: () => {
      if (!live) return
      setShare(live.share)
      if (live.rangeBps !== null) setRange(live.rangeBps)
    },
    toggle: () => setShown((value) => !value),
    setShare: (value) =>
      setShare(
        Number.isFinite(value)
          ? Math.max(0, Math.min(10000, Math.round(value)))
          : 0
      ),
    setRange: (value) =>
      setRange(
        Number.isFinite(value) ? Math.max(0, Math.min(10000, value)) : 0
      ),
  }
  const titleId = useId()
  const conflict = leanConflict(draft, selected, vault)
  const blockedReason = selected === 'liquidity' ? liquidityRefusal : null
  return (
    <section
      aria-labelledby={titleId}
      className="border-line-soft bg-surface @container rounded-xl border"
    >
      <header className="border-line-soft flex items-center justify-between gap-3 border-b px-5 py-4">
        <div>
          <h3 id={titleId} className="text-sm font-bold">
            {moduleNames[selected]} settings
          </h3>
          <p className="text-muted mt-1 text-xs">
            Changes apply only to this module.
          </p>
        </div>
        <ModuleSwitch
          label={`Enable ${moduleNames[selected].toLowerCase()}`}
          checked={draft[selected].enabled}
          disabled={
            !draft[selected].enabled &&
            (conflict !== null || blockedReason !== null)
          }
          onChange={(enabled) =>
            onChange({ ...draft, [selected]: { ...draft[selected], enabled } })
          }
        />
      </header>
      <ModuleEditor
        module={selected}
        draft={draft}
        onChange={onChange}
        currency={currency}
        settlement={settlement}
        corridor={corridor}
        example={example}
        blockedReason={blockedReason}
        conflict={conflict}
        view={view}
        now={now}
      />
    </section>
  )
}

function ModuleEditor({
  module,
  draft,
  onChange,
  currency,
  settlement,
  corridor,
  example,
  blockedReason,
  conflict,
  view,
  now,
}: {
  module: ModuleKey
  draft: ModulesConfig
  onChange: (config: ModulesConfig) => void
  currency: string
  settlement: string
  corridor: QuoteSides | null
  example: Example
  /** Why this module can't run on this bot, or null. */
  blockedReason: string | null
  /** The other inventory module is on, so this one can't be turned on. */
  conflict: string | null
  view: ModulesView
  now: number
}) {
  const guide = moduleGuides[module]
  return (
    <>
      <div className="border-line-soft flex flex-wrap items-center gap-x-4 gap-y-2 border-b px-5 py-2.5">
        {(module === 'inventory' || module === 'spreads') && (
          <ModulePresets
            draft={draft}
            onChange={onChange}
            module={module}
            currency={currency}
            settlement={settlement}
          />
        )}
        {blockedReason !== null ? (
          <p role="note" className="text-xs font-bold text-warning">
            {blockedReason.trim().replace(/\.?$/, '.')}{' '}
            {draft[module].enabled
              ? 'Turn the schedule off; the bot will not act on this vault.'
              : 'The schedule can only be turned on for a vault with a yield adapter.'}
          </p>
        ) : conflict !== null && !draft[module].enabled ? (
          <p role="note" className="text-xs font-bold text-warning">
            {conflict}
          </p>
        ) : (
          !draft[module].enabled && (
            <p className="text-muted text-xs">
              Off. You can prepare its settings before turning it on.
            </p>
          )
        )}
        <Popover
          align="end"
          className="ml-auto"
          label={
            <>
              {guide.title} <span aria-hidden>›</span>
            </>
          }
          buttonClassName="text-muted hover:text-ink text-xs font-bold"
        >
          {() => (
            <div className="text-muted p-4 text-sm leading-relaxed">
              {guide.body(currency, settlement)}
            </div>
          )}
        </Popover>
      </div>
      <div className="@min-[52rem]:grid-cols-[minmax(0,1fr)_auto] grid grid-cols-1">
        <div className="min-w-0 p-5 sm:p-6">
          {/* Caps line length when the example is tucked away. */}
          <div className="max-w-3xl space-y-6">
            {module === 'curve' ? (
              <CurveFields
                draft={draft}
                onChange={onChange}
                view={view}
                now={now}
                currency={currency}
                settlement={settlement}
                example={
                  example.shown
                    ? {
                        share: example.share / 10_000,
                        sizeUsd: example.sizeUsd,
                      }
                    : null
                }
              />
            ) : module === 'profit_unwind' ? (
              <ProfitUnwindFields
                draft={draft}
                onChange={onChange}
                view={view}
                now={now}
                currency={currency}
                settlement={settlement}
              />
            ) : (
              <ModuleFields
                module={module}
                draft={draft}
                onChange={onChange}
                currency={currency}
                settlement={settlement}
              />
            )}
          </div>
        </div>
        <ModuleExample
          module={module}
          draft={draft}
          currency={currency}
          settlement={settlement}
          corridor={corridor}
          example={example}
          view={view}
          now={now}
        />
      </div>
    </>
  )
}

/** A vault module's own fields. The inventory curve and profit unwind have
 * their own editors. */
function ModuleFields({
  module,
  ...props
}: {
  module: Exclude<ModuleKey, 'curve' | 'profit_unwind'>
  draft: ModulesConfig
  onChange: (config: ModulesConfig) => void
  currency: string
  settlement: string
}) {
  const Fields = {
    inventory: InventoryFields,
    spreads: SpreadFields,
    rebalance: RebalanceFields,
    liquidity: LiquidityFields,
  }[module]
  return <Fields {...props} />
}

function ModuleExample({
  module,
  draft,
  currency,
  settlement,
  corridor,
  example,
  view,
  now,
}: {
  module: ModuleKey
  draft: ModulesConfig
  currency: string
  settlement: string
  corridor: QuoteSides | null
  example: Example
  view: ModulesView
  now: number
}) {
  const id = useId()
  const valid =
    moduleParameterErrors(
      draft,
      view.vault ?? true
    ).length === 0
  const enabled = draft[module].enabled
  const breakdown =
    valid && corridor
      ? quoteBreakdown(draft, corridor, example.share, example.range)
      : null
  const notes = [
    quoteModeNote(draft.mode),
    valid && corridor
      ? warmupNote(draft, corridor, example.share, example.range)
      : null,
  ].filter((note): note is string => note !== null)
  const sale =
    enabled && example.share >= draft.rebalance.trigger_bps
      ? Math.max(
          0,
          Math.min(
            draft.rebalance.max_trade_bps,
            example.share - draft.inventory.target_bps
          )
        )
      : 0
  const label = 'Simulate'
  return (
    <div className="border-line-soft bg-canvas @min-[52rem]:flex-row @min-[52rem]:border-l @min-[52rem]:border-t-0 @min-[52rem]:group-last/row:rounded-bl-none flex min-w-0 flex-col border-t group-last/row:rounded-b-xl">
      <button
        type="button"
        aria-expanded={example.shown}
        aria-controls={id}
        onClick={example.toggle}
        className={`hover:bg-hover group/bar @min-[52rem]:hidden flex w-full items-center justify-between gap-3 p-5 text-left text-sm font-bold ${example.shown ? '' : 'group-last/row:rounded-b-xl'}`}
      >
        <span className={example.shown ? 'text-accent' : ''}>{label}</span>
        <Chevron
          className={`text-muted group-hover/bar:text-ink transition-transform duration-300 motion-reduce:transition-none ${example.shown ? '' : 'rotate-180'}`}
        />
      </button>
      {/* Desktop: a narrow tab that pulls the example out to its right and
          stays in place to push it back, so focus never moves. */}
      <button
        type="button"
        aria-expanded={example.shown}
        aria-controls={id}
        aria-label={`${example.shown ? 'Hide' : 'Show'} simulation`}
        title={`${example.shown ? 'Hide' : 'Show'} simulation`}
        onClick={example.toggle}
        className={`hover:bg-hover group/tab @min-[52rem]:flex @min-[52rem]:group-last/row:rounded-br-xl hidden w-11 shrink-0 flex-col items-center gap-3 py-5 transition-colors ${example.shown ? 'border-line-soft @min-[52rem]:group-last/row:rounded-br-none border-r' : ''}`}
      >
        <Chevron
          className={`text-muted group-hover/tab:text-ink ${example.shown ? '-rotate-90' : 'rotate-90'}`}
        />
        <span
          aria-hidden
          className={`rotate-180 text-[11px] font-bold uppercase tracking-wider [writing-mode:vertical-rl] ${example.shown ? 'text-accent' : 'text-muted group-hover/tab:text-ink'}`}
        >
          {label}
        </span>
      </button>
      {/* Narrow: the example grows open under the bar and rises into place.
          Wide: it slides out to the right of the tab. */}
      <div
        className={`@min-[52rem]:grid-rows-[1fr] grid transition-[grid-template-rows,width] duration-300 ease-out motion-reduce:transition-none ${example.shown ? '@min-[52rem]:w-[22rem] grid-rows-[1fr]' : '@min-[52rem]:w-0 grid-rows-[0fr]'}`}
      >
        <div className="min-h-0 overflow-hidden">
          {example.shown && (
            <aside
              id={id}
              aria-label={label}
              className="border-line-soft @min-[52rem]:w-[22rem] @min-[52rem]:border-t-0 @min-[52rem]:animate-none animate-rise-in border-t p-5 motion-reduce:animate-none sm:p-6"
            >
              <h4 className="mb-6 text-lg font-bold">
                {module === 'inventory'
                  ? 'See your quotes respond'
                  : module === 'spreads'
                    ? 'See the extra margin'
                    : module === 'liquidity'
                      ? 'See the weekly schedule'
                      : module === 'curve' || module === 'profit_unwind'
                        ? 'See a quote'
                        : 'See when a sale starts'}
              </h4>
              {module === 'curve' ? (
                <CurveExample
                  draft={draft}
                  view={view}
                  now={now}
                  currency={currency}
                  settlement={settlement}
                  example={{
                    share: example.share / 10_000,
                    sizeUsd: example.sizeUsd,
                  }}
                  onExample={(next) => {
                    example.setShare(Math.round(next.share * 10_000))
                    example.setSizeUsd(next.sizeUsd)
                  }}
                />
              ) : module === 'profit_unwind' ? (
                <UnwindExample
                  draft={draft}
                  view={view}
                  now={now}
                  currency={currency}
                  settlement={settlement}
                  example={{
                    share: example.share / 10_000,
                    sizeUsd: example.sizeUsd,
                  }}
                  onExample={(next) => {
                    example.setShare(Math.round(next.share * 10_000))
                    example.setSizeUsd(next.sizeUsd)
                  }}
                />
              ) : module === 'liquidity' ? (
                liquidityScheduleErrors(draft.liquidity).length === 0 ? (
                  <div className="space-y-5">
                    <ScheduleStrip schedule={draft.liquidity} />
                    <p className="text-muted text-xs leading-relaxed">
                      Bundled fills can only use {settlement} kept liquid up to
                      the floor. The rest earns yield in Aave and is still
                      usable by single-order fills through the executor.
                    </p>
                  </div>
                ) : (
                  <p className="text-muted text-sm">
                    Correct the schedule to update this example.
                  </p>
                )
              ) : !valid ? (
                <p className="text-muted text-sm">
                  Correct the settings to update this example.
                </p>
              ) : (
                <div className="space-y-6">
                  <AllocationBar
                    share={example.share}
                    target={draft.inventory.target_bps}
                    limit={draft.inventory.max_bps}
                    currency={currency}
                  />
                  {!draft.inventory.enabled && (
                    <p className="text-muted text-xs">
                      Inventory balancing is off. Its target and purchase limit
                      are shown for reference.
                    </p>
                  )}
                  {module === 'rebalance' && (
                    <div className="border-line-soft bg-surface rounded-xl border p-4">
                      <p className="text-muted text-sm">
                        {sale > 0 ? 'Sale size · up to' : 'No sale requested'}
                      </p>
                      <p className="my-2 text-3xl font-bold">
                        {percentage(sale)}
                      </p>
                      <p className="text-muted text-xs leading-relaxed">
                        {sale > 0
                          ? `of vault value, converted from ${currency} to ${settlement}. Actual size is also limited by available funds and the vault’s order cap.`
                          : enabled
                            ? `Sales start at ${percentage(draft.rebalance.trigger_bps)} ${currency} holdings.`
                            : 'Enable spot rebalancing to request sales.'}
                      </p>
                    </div>
                  )}
                  {breakdown ? (
                    <div className="space-y-3">
                      <QuoteBreakdown
                        rows={breakdown}
                        highlight={
                          module === 'inventory' || module === 'spreads'
                            ? module
                            : null
                        }
                        current={
                          example.atLive && example.live
                            ? liveRow(example.live)
                            : null
                        }
                      />
                      <p className="text-muted text-xs leading-relaxed">
                        Modules start from your corridor spread and adjust it;
                        they don't replace it.
                      </p>
                      {notes.map((note) => (
                        <p
                          key={note}
                          className="text-muted text-xs leading-relaxed"
                        >
                          {note}
                        </p>
                      ))}
                    </div>
                  ) : (
                    <p className="text-muted text-xs leading-relaxed">
                      Your corridor spread is an absolute offset, so it needs a
                      price from the running bot before the full spread can be
                      shown here.
                    </p>
                  )}
                  {example.live &&
                    (example.atLive ? (
                      <p className="text-muted text-xs leading-relaxed">
                        Holdings and price range below are the bot's current
                        readings.
                      </p>
                    ) : (
                      <button
                        type="button"
                        onClick={example.useLive}
                        className="text-left text-xs font-bold text-accent hover:underline"
                      >
                        Use the bot's current {currency} holdings
                        {example.live.rangeBps === null
                          ? ''
                          : ' and price range'}
                      </button>
                    ))}
                  {(module === 'spreads' || draft.spreads.enabled) && (
                    <NumberControl
                      label="Example price range"
                      value={example.range}
                      sliderMax={100}
                      max={10000}
                      fractionDigits={4}
                      onChange={example.setRange}
                    />
                  )}
                  <div className="border-line-soft border-t pt-5">
                    <NumberControl
                      label={`Example ${currency} holdings`}
                      value={example.share}
                      max={10000}
                      sliderMax={10000}
                      onChange={example.setShare}
                    />
                  </div>
                </div>
              )}
              <p className="text-muted mt-6 text-xs leading-relaxed">
                {module === 'liquidity'
                  ? 'Illustration only. The floor is a share of free settlement, so the amount kept liquid moves with the vault balance.'
                  : module === 'curve' || module === 'profit_unwind'
                    ? 'Illustration only, not a live quote. Open quotes on the same side, capacity and the venue fee on the actual request still apply.'
                    : 'Illustration only, not a live quote or a return forecast. Open quotes, available funds and vault limits can still hold a side back.'}
              </p>
            </aside>
          )}
        </div>
      </div>
    </div>
  )
}

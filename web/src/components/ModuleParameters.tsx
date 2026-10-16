import { useId, useState, type ReactNode } from 'react'
import type { ModulesConfig } from '../modules'
import ModulePresets from './ModulePresets'
import {
  moduleKeyFigures,
  moduleKeys,
  moduleModes,
  moduleNames,
  moduleParameterErrors,
  percentage,
  quoteBreakdown,
  quoteModeNote,
  type ModuleKey,
  type QuoteSides,
} from '../modulePresentation'
import { matchingPreset } from '../modulePresets'
import {
  AllocationBar,
  Chevron,
  ModuleSwitch,
  NumberControl,
  Popover,
  QuoteBreakdown,
} from './ModuleControls'
import {
  InventoryFields,
  moduleGuides,
  RebalanceFields,
  SpreadFields,
} from './ModuleParameterFields'
import { Button } from './ui'

type Example = {
  share: number
  range: number
  setShare: (bps: number) => void
  setRange: (bps: number) => void
  /** Collapsed to a narrow tab by default; shared by all modules. */
  shown: boolean
  toggle: () => void
}

/** The whole module config as one strategy: run mode in the header, then a
 * row per module with its switch, two defining figures and preset. Edit opens
 * that module's values in place; one module is open at a time. */
export default function ModuleParameters({
  draft,
  onChange,
  selected,
  onSelect,
  currency,
  settlement,
  corridor,
  initialShare,
}: {
  draft: ModulesConfig
  onChange: (config: ModulesConfig) => void
  selected: ModuleKey | null
  onSelect: (key: ModuleKey | null) => void
  currency: string
  settlement: string
  /** The corridor's saved spreads in bps; null until they can be read. */
  corridor: QuoteSides | null
  initialShare?: number | null
}) {
  const [share, setShare] = useState(initialShare ?? 4500)
  const [range, setRange] = useState(20)
  const [shown, setShown] = useState(false)
  const example: Example = {
    share,
    range,
    shown,
    toggle: () => setShown((value) => !value),
    setShare: (value) =>
      setShare(
        Number.isFinite(value)
          ? Math.max(0, Math.min(10000, Math.round(value)))
          : 0
      ),
    setRange: (value) =>
      setRange(
        Number.isFinite(value)
          ? Math.max(0, Math.min(1000, Math.round(value)))
          : 0
      ),
  }
  const titleId = useId()
  const descriptions: Record<ModuleKey, string> = {
    inventory: `Steer ${currency} holdings toward a target by shifting quotes.`,
    spreads: 'Widen margins when prices move, up to a cap you set.',
    rebalance: `Suggest selling excess ${currency} for ${settlement}.`,
  }
  return (
    <section
      aria-labelledby={titleId}
      className="border-line-soft bg-surface rounded-xl border"
    >
      <header className="border-line-soft flex flex-wrap items-center justify-between gap-4 border-b px-5 py-4">
        <div className="min-w-0">
          <h3 id={titleId} className="font-bold">
            Quote strategy
          </h3>
          <p className="text-muted mt-0.5 text-xs">
            {moduleModes.find((m) => m.value === draft.mode)?.effect}
          </p>
        </div>
        <ModeSwitch
          value={draft.mode}
          onChange={(mode) => onChange({ ...draft, mode })}
        />
      </header>
      {moduleKeys.map((key) => (
        <ModuleRow
          key={key}
          module={key}
          draft={draft}
          onChange={onChange}
          description={descriptions[key]}
          open={selected === key}
          onToggle={() => onSelect(selected === key ? null : key)}
        >
          <ModuleEditor
            module={key}
            draft={draft}
            onChange={onChange}
            currency={currency}
            settlement={settlement}
            corridor={corridor}
            example={example}
          />
        </ModuleRow>
      ))}
    </section>
  )
}

const modeDots: Record<ModulesConfig['mode'], string> = {
  off: 'bg-faint',
  shadow: 'bg-warning',
  live: 'bg-success',
}

function ModeSwitch({
  value,
  onChange,
}: {
  value: ModulesConfig['mode']
  onChange: (mode: ModulesConfig['mode']) => void
}) {
  return (
    <div
      role="radiogroup"
      aria-label="Module mode"
      className="bg-active inline-flex rounded-lg p-0.5"
    >
      {moduleModes.map((mode) => (
        <label
          key={mode.value}
          title={mode.hint}
          className={`inline-flex cursor-pointer items-center gap-2 rounded-md px-3 py-1.5 text-sm font-bold transition has-[:focus-visible]:outline has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-offset-2 has-[:focus-visible]:outline-accent ${value === mode.value ? 'bg-canvas text-ink ring-line-soft shadow-sm ring-1' : 'text-muted hover:text-ink'}`}
        >
          <input
            type="radio"
            name="module-mode"
            value={mode.value}
            checked={value === mode.value}
            onChange={() => onChange(mode.value)}
            className="sr-only"
          />
          <span
            aria-hidden
            className={`size-1.5 rounded-full ${modeDots[mode.value]}`}
          />
          {mode.label}
          <span className="sr-only">: {mode.hint}</span>
        </label>
      ))}
    </div>
  )
}

function ModuleRow({
  module,
  draft,
  onChange,
  description,
  open,
  onToggle,
  children,
}: {
  module: ModuleKey
  draft: ModulesConfig
  onChange: (config: ModulesConfig) => void
  description: string
  open: boolean
  onToggle: () => void
  children: ReactNode
}) {
  const id = useId()
  const on = draft[module].enabled
  const name = moduleNames[module]
  const figures = moduleKeyFigures(draft, module)
  const preset =
    module === 'rebalance'
      ? null
      : (matchingPreset(draft, module)?.name ?? 'Custom')
  const caption = 'text-faint text-[11px] font-bold uppercase tracking-wider'
  return (
    // The card can't clip (popovers must escape it), so the last row's
    // example panel rounds its own bottom corners via group-last/row.
    <div className="group/row border-line-soft border-b last:border-b-0">
      <div
        className={`relative grid grid-cols-[auto_minmax(0,1fr)_auto] items-center gap-x-4 px-5 py-3.5 md:grid-cols-[auto_minmax(0,1fr)_6.5rem_6.5rem_6rem_auto] md:gap-x-5 ${open ? 'bg-hover' : ''}`}
      >
        {open && (
          <span
            aria-hidden
            className="absolute inset-y-0 left-0 w-0.5 bg-accent"
          />
        )}
        <ModuleSwitch
          label={`Enable ${name.toLowerCase()}`}
          checked={on}
          showState={false}
          onChange={(enabled) =>
            onChange({ ...draft, [module]: { ...draft[module], enabled } })
          }
        />
        <div className="min-w-0">
          <h4 className={`text-sm font-bold ${on ? '' : 'text-muted'}`}>
            {name}
          </h4>
          <p className="text-muted text-xs md:truncate">{description}</p>
          <p className="text-muted mt-1 text-xs tabular-nums md:hidden">
            {figures.map((f) => `${f.label} ${f.value}`).join(' · ')}
          </p>
        </div>
        {figures.map((f) => (
          <div key={f.label} className="hidden md:block">
            <p className={caption}>{f.label}</p>
            <p
              className={`text-sm font-bold tabular-nums ${on ? '' : 'text-muted'}`}
            >
              {f.value}
            </p>
          </div>
        ))}
        <div className="hidden md:block">
          {preset && (
            <>
              <p className={caption}>Preset</p>
              <p className="text-muted text-sm">{preset}</p>
            </>
          )}
        </div>
        <Button
          variant={open ? 'secondary' : 'ghost'}
          aria-expanded={open}
          aria-controls={open ? id : undefined}
          aria-label={`${open ? 'Close' : 'Edit'} ${name.toLowerCase()}`}
          onClick={onToggle}
        >
          {open ? 'Close' : 'Edit'}
          <Chevron
            className={`transition-transform ${open ? 'rotate-180' : ''}`}
          />
        </Button>
      </div>
      {open && (
        <div
          id={id}
          role="region"
          aria-label={`${name} settings`}
          className="border-line-soft border-t"
        >
          {children}
        </div>
      )}
    </div>
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
}: {
  module: ModuleKey
  draft: ModulesConfig
  onChange: (config: ModulesConfig) => void
  currency: string
  settlement: string
  corridor: QuoteSides | null
  example: Example
}) {
  const Fields = {
    inventory: InventoryFields,
    spreads: SpreadFields,
    rebalance: RebalanceFields,
  }[module]
  const guide = moduleGuides[module]
  return (
    <>
      <div className="border-line-soft flex flex-wrap items-center gap-x-4 gap-y-2 border-b px-5 py-2.5">
        {module !== 'rebalance' && (
          <ModulePresets
            draft={draft}
            onChange={onChange}
            module={module}
            currency={currency}
            settlement={settlement}
          />
        )}
        {!draft[module].enabled && (
          <p className="text-muted text-xs">
            Off. You can prepare its settings before turning it on.
          </p>
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
              {guide.body(currency)}
            </div>
          )}
        </Popover>
      </div>
      <div className="grid lg:grid-cols-[minmax(0,1fr)_auto]">
        <div className="p-5 sm:p-6">
          {/* Caps line length when the example is tucked away. */}
          <div className="max-w-3xl space-y-6">
            <Fields draft={draft} onChange={onChange} currency={currency} />
          </div>
        </div>
        <ModuleExample
          module={module}
          draft={draft}
          currency={currency}
          settlement={settlement}
          corridor={corridor}
          example={example}
        />
      </div>
    </>
  )
}

function ModuleExample({
  module,
  draft,
  currency,
  settlement,
  corridor,
  example,
}: {
  module: ModuleKey
  draft: ModulesConfig
  currency: string
  settlement: string
  corridor: QuoteSides | null
  example: Example
}) {
  const id = useId()
  const valid = moduleParameterErrors(draft).length === 0
  const enabled = draft[module].enabled
  const breakdown =
    valid && corridor
      ? quoteBreakdown(draft, corridor, example.share, example.range)
      : null
  const modeNote = quoteModeNote(draft.mode)
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
    <div className="border-line-soft bg-canvas flex min-w-0 flex-col border-t group-last/row:rounded-b-xl lg:flex-row lg:border-l lg:border-t-0 lg:group-last/row:rounded-bl-none">
      <button
        type="button"
        aria-expanded={example.shown}
        aria-controls={id}
        onClick={example.toggle}
        className="hover:bg-hover flex w-full items-center justify-between gap-3 p-5 text-left text-sm font-bold group-last/row:rounded-b-xl lg:hidden"
      >
        {label} <span aria-hidden>{example.shown ? '−' : '+'}</span>
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
        className={`hover:bg-hover group/tab hidden w-11 shrink-0 flex-col items-center gap-3 py-5 transition-colors lg:flex lg:group-last/row:rounded-br-xl ${example.shown ? 'border-line-soft border-r lg:group-last/row:rounded-br-none' : ''}`}
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
      <div
        className={`transition-[width] duration-200 ease-out motion-reduce:transition-none lg:overflow-hidden ${example.shown ? 'lg:w-[22rem]' : 'lg:w-0'}`}
      >
        {example.shown && (
          <aside
            id={id}
            aria-label={label}
            className="border-line-soft border-t p-5 sm:p-6 lg:w-[22rem] lg:border-t-0"
          >
            <h4 className="mb-6 text-lg font-bold">
              {module === 'inventory'
                ? 'See your quotes respond'
                : module === 'spreads'
                  ? 'See the extra margin'
                  : 'See when a sale starts'}
            </h4>
            {!valid ? (
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
                      highlight={module === 'rebalance' ? null : module}
                    />
                    {modeNote && (
                      <p className="text-muted text-xs leading-relaxed">
                        {modeNote}
                      </p>
                    )}
                  </div>
                ) : (
                  <p className="text-muted text-xs leading-relaxed">
                    Your corridor spread is an absolute offset, so it needs a
                    price from the running bot before the full spread can be
                    shown here.
                  </p>
                )}
                {(module === 'spreads' || draft.spreads.enabled) && (
                  <NumberControl
                    label="Example price range"
                    value={example.range}
                    sliderMax={100}
                    max={1000}
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
              Illustration only, not a live quote or a return forecast. Open
              quotes, available funds and vault limits can still hold a side
              back.
            </p>
          </aside>
        )}
      </div>
    </div>
  )
}

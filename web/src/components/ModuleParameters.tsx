import { useId, useState } from 'react'
import type { ModulesConfig } from '../modules'
import {
  inventoryIllustration,
  moduleKeys,
  moduleNames,
  moduleParameterErrors,
  percentage,
  volatilityIllustration,
  type ModuleKey,
} from '../modulePresentation'
import {
  AllocationBar,
  MarginBars,
  ModuleSwitch,
  NumberControl,
} from './ModuleControls'
import {
  InventoryFields,
  SpreadFields,
  RebalanceFields,
} from './ModuleParameterFields'

export default function ModuleParameters({
  draft,
  onChange,
  selected,
  onSelect,
  currency,
  settlement,
  initialShare,
}: {
  draft: ModulesConfig
  onChange: (config: ModulesConfig) => void
  selected: ModuleKey
  onSelect: (key: ModuleKey) => void
  currency: string
  settlement: string
  initialShare?: number | null
}) {
  const [exampleShare, setExampleShare] = useState(initialShare ?? 4500)
  const [exampleRange, setExampleRange] = useState(20)
  const [showExample, setShowExample] = useState(false)
  const exampleId = useId()
  const update = <K extends keyof ModulesConfig>(
    key: K,
    value: ModulesConfig[K]
  ) => onChange({ ...draft, [key]: value })
  const valid = moduleParameterErrors(draft).length === 0
  const enabled = draft[selected].enabled
  const ModuleFields = {
    inventory: InventoryFields,
    spreads: SpreadFields,
    rebalance: RebalanceFields,
  }[selected]
  const explanations = {
    inventory: `Use your quotes to bring ${currency} holdings toward a target.`,
    spreads: 'Add a margin when prices move, within a limit you choose.',
    rebalance: `Request a sale of excess ${currency} for ${settlement}.`,
  }
  const extra = valid
    ? volatilityIllustration(draft, exampleShare, exampleRange)
    : null
  const margins = valid ? inventoryIllustration(draft, exampleShare) : null
  const sale =
    enabled && exampleShare >= draft.rebalance.trigger_bps
      ? Math.max(
          0,
          Math.min(
            draft.rebalance.max_trade_bps,
            exampleShare - draft.inventory.target_bps
          )
        )
      : 0

  return (
    <div className="space-y-5">
      <div className="border-line-soft bg-surface rounded-xl border p-5">
        <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
          <h3 className="font-bold">How modules run</h3>
          <span className="text-muted text-xs">
            Applies to all enabled modules
          </span>
        </div>
        <div
          role="radiogroup"
          aria-label="Module mode"
          className="grid grid-cols-3 gap-2"
        >
          {(
            [
              ['off', 'Off', 'Regular quotes'],
              ['shadow', 'Preview', 'Observe only'],
              ['live', 'Live', 'Apply to quotes'],
            ] as const
          ).map(([mode, label, hint]) => (
            <label
              key={mode}
              className={`relative cursor-pointer rounded-xl border p-3 text-center transition has-[:focus-visible]:outline has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-offset-2 has-[:focus-visible]:outline-accent ${draft.mode === mode ? 'bg-accent-tint border-accent' : 'border-line-soft hover:bg-canvas'}`}
            >
              <input
                type="radio"
                name="module-mode"
                value={mode}
                checked={draft.mode === mode}
                onChange={() => update('mode', mode)}
                className="peer sr-only"
              />
              <span className="block text-sm font-bold peer-focus-visible:underline">
                {label}
              </span>
              <span className="text-muted mt-1 block text-xs">{hint}</span>
            </label>
          ))}
        </div>
        <p className="text-muted mt-3 text-xs">
          {draft.mode === 'live'
            ? 'Saving applies these settings to live quotes and restarts a running bot.'
            : draft.mode === 'shadow'
              ? 'Preview records proposed adjustments without changing live quotes.'
              : 'Saving turns off new module activity. Already signed orders remain valid until they expire.'}
        </p>
      </div>

      <div>
        <p className="text-muted mb-3 text-xs font-bold uppercase tracking-wider">
          Choose a module to configure
        </p>
        <div className="grid gap-2 sm:grid-cols-3" aria-label="Choose a module">
          {moduleKeys.map((key, i) => (
            <button
              key={key}
              aria-pressed={selected === key}
              onClick={() => onSelect(key)}
              className={`flex items-center gap-3 rounded-xl border p-4 text-left transition ${selected === key ? 'bg-accent-tint border-accent' : 'border-line-soft bg-surface hover:bg-canvas'}`}
            >
              <span
                aria-hidden
                className={`text-xs font-bold ${selected === key ? 'text-accent' : 'text-muted'}`}
              >
                0{i + 1}
              </span>
              <span className="flex-1 text-sm font-bold">
                {moduleNames[key]}
              </span>
              <span
                className={`size-2 rounded-full ${draft[key].enabled ? 'bg-accent' : 'bg-active'}`}
                aria-label={draft[key].enabled ? 'Enabled' : 'Off'}
              />
            </button>
          ))}
        </div>
      </div>

      <section
        aria-label={`${moduleNames[selected]} settings`}
        className="border-line-soft bg-surface overflow-hidden rounded-xl border"
      >
        <div className="border-line-soft flex items-start justify-between gap-4 border-b p-5">
          <div>
            <h3 className="text-lg font-bold">{moduleNames[selected]}</h3>
            <p className="text-muted mt-1 text-sm">{explanations[selected]}</p>
          </div>
          <ModuleSwitch
            label={`Enable ${moduleNames[selected].toLowerCase()}`}
            checked={enabled}
            onChange={(value) =>
              update(selected, { ...draft[selected], enabled: value })
            }
          />
        </div>
        {!enabled && (
          <p className="bg-hover mx-5 mt-4 rounded-lg px-3 py-2 text-sm">
            This module is off. You can prepare its settings before enabling it.
          </p>
        )}
        <div className="grid lg:grid-cols-[1.15fr_1fr]">
          <div className="space-y-6 p-5 sm:p-6">
            <ModuleFields
              key={selected}
              draft={draft}
              onChange={onChange}
              currency={currency}
            />
          </div>
          <div className="min-w-0">
            <button
              aria-expanded={showExample}
              aria-controls={exampleId}
              onClick={() => setShowExample((value) => !value)}
              className="border-line-soft bg-canvas flex w-full items-center justify-between gap-3 border-t p-5 text-left text-sm font-bold lg:hidden"
            >
              Try an interactive example{' '}
              <span aria-hidden>{showExample ? '−' : '+'}</span>
            </button>
            <aside
              id={exampleId}
              className={`border-line-soft bg-canvas h-full border-t p-5 sm:p-6 lg:block lg:border-l lg:border-t-0 ${showExample ? 'block' : 'hidden'}`}
              aria-label="Interactive example"
            >
              <p className="text-xs font-bold uppercase tracking-wider text-accent">
                Try an example
              </p>
              <h4 className="mb-2 mt-2 text-lg font-bold">
                {selected === 'inventory'
                  ? 'See your quotes respond'
                  : selected === 'spreads'
                    ? 'See the extra margin'
                    : 'See when a sale starts'}
              </h4>
              <p className="text-muted mb-6 text-xs leading-relaxed">
                Illustration of this module’s settings. This is not a live quote
                or a return forecast.
              </p>
              {!valid ? (
                <p className="text-muted text-sm">
                  Correct the settings below to update this example.
                </p>
              ) : (
                <div className="space-y-6">
                  <AllocationBar
                    share={exampleShare}
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
                  {selected === 'inventory' && margins && (
                    <>
                      <MarginBars {...margins} />
                      <p className="text-muted text-xs leading-relaxed">
                        Example base margins: 0.20% each. Includes inventory
                        adjustments and the minimum margin; excludes dynamic
                        spreads.
                      </p>
                    </>
                  )}
                  {selected === 'spreads' && extra && (
                    <>
                      <MarginBars {...extra} extra />
                      <p className="text-muted text-xs leading-relaxed">
                        Extra margin only. Added after inventory adjustments.
                        Actual quotes also depend on available funds and side
                        limits.
                      </p>
                      <NumberControl
                        label="Example price range"
                        value={exampleRange}
                        sliderMax={100}
                        max={1000}
                        onChange={(value) =>
                          setExampleRange(
                            Number.isFinite(value)
                              ? Math.max(0, Math.min(1000, Math.round(value)))
                              : 0
                          )
                        }
                      />
                    </>
                  )}
                  {selected === 'rebalance' && (
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
                  <div className="border-line-soft border-t pt-5">
                    <NumberControl
                      label={`Example ${currency} holdings`}
                      value={exampleShare}
                      max={10000}
                      sliderMax={10000}
                      onChange={(value) =>
                        setExampleShare(
                          Number.isFinite(value)
                            ? Math.max(0, Math.min(10000, Math.round(value)))
                            : 0
                        )
                      }
                    />
                  </div>
                </div>
              )}
              <p className="text-muted mt-6 text-xs leading-relaxed">
                Only the example changes here. Use historical simulation to test
                your settings on past activity.
              </p>
            </aside>
          </div>
        </div>
      </section>
    </div>
  )
}

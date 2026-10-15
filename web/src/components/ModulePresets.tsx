import type { ModulesConfig } from '../modules'
import { moduleNames, percentage } from '../modulePresentation'
import {
  applyPreset,
  matchingPreset,
  modulePresets,
  presetConflict,
  presetSummary,
  type PresetModule,
} from '../modulePresets'
import { Chevron, Popover } from './ModuleControls'

/** Preset picker for one module: a compact button showing the matching preset
 * (or Custom) that opens the three options, each with its values, tradeoff or
 * conflict. Choosing one changes this module's draft only. */
export default function ModulePresets({
  draft,
  onChange,
  module,
  currency,
  settlement,
}: {
  draft: ModulesConfig
  onChange: (config: ModulesConfig) => void
  module: PresetModule
  currency: string
  settlement: string
}) {
  const selected = matchingPreset(draft, module)
  return (
    <Popover
      label={
        <>
          <span className="text-muted">Preset</span>
          {selected?.name ?? 'Custom'}
          <Chevron className="text-muted" />
        </>
      }
      buttonClassName="border-line bg-canvas hover:bg-hover inline-flex items-center gap-2 rounded-lg border px-2.5 py-1 text-xs font-bold"
    >
      {(close) => (
        <>
          <div role="group" aria-label={`${moduleNames[module]} presets`}>
            {modulePresets.map((preset) => {
              const conflict = presetConflict(draft, module, preset)
              const active = selected?.id === preset.id
              return (
                <button
                  key={preset.id}
                  type="button"
                  aria-pressed={active}
                  disabled={!!conflict}
                  onClick={() => {
                    onChange(applyPreset(draft, module, preset))
                    close()
                  }}
                  className={`border-line-soft flex w-full gap-3 border-b px-4 py-3 text-left transition disabled:cursor-not-allowed ${active ? 'bg-accent-tint' : 'hover:bg-hover disabled:hover:bg-transparent'}`}
                >
                  <span
                    aria-hidden
                    className="w-3 shrink-0 pt-0.5 text-xs text-accent"
                  >
                    {active ? '✓' : ''}
                  </span>
                  <span className="min-w-0">
                    <span className="block text-sm font-bold">
                      {preset.name}
                    </span>
                    <span className="text-muted mt-0.5 block text-xs tabular-nums">
                      {presetSummary(preset, module)}
                    </span>
                    <span
                      className={`mt-1 block text-xs ${conflict ? 'text-warning' : 'text-muted'}`}
                    >
                      {conflict ?? preset.tradeoff[module]}
                    </span>
                  </span>
                </button>
              )
            })}
          </div>
          <div className="bg-canvas text-muted space-y-2 px-4 py-3 text-xs leading-relaxed">
            <p>
              Changes this module’s draft only.
              {module === 'inventory'
                ? ` Your minimum margin stays at ${Number.isFinite(draft.inventory.spread_floor_bps) ? percentage(draft.inventory.spread_floor_bps) : 'your custom value'}.`
                : ' All presets favor trades toward your inventory target and use a 2-minute warmup.'}
            </p>
            <details>
              <summary className="cursor-pointer font-bold text-accent">
                How these presets were chosen
              </summary>
              <PresetRationale currency={currency} settlement={settlement} />
            </details>
          </div>
        </>
      )}
    </Popover>
  )
}

function PresetRationale({
  currency,
  settlement,
}: {
  currency: string
  settlement: string
}) {
  const cngnUsdt =
    currency.toLowerCase() === 'cngn' && settlement.toLowerCase() === 'usdt'
  return (
    <div className="mt-2 space-y-2">
      <p>
        Professional market makers shift quotes to manage inventory and widen
        margins as price risk rises. These are Textile starting points based on
        those principles, not a prediction of returns.
      </p>
      <p>
        {cngnUsdt ? 'Checked against' : 'The reference check used'} 1,440
        recorded cNGN/USDT prices over 6–7 Oct 2026 (UTC). Updates were
        typically a minute apart. The 95th-percentile price ranges were 0.043%
        over 5 minutes, 0.079% over 15 minutes and 0.103% over 30 minutes.
        {cngnUsdt
          ? ' This was one day with only five vault trades.'
          : ' These numbers are not a calibration for your corridor.'}
      </p>
      <p>
        Inventory targets are risk preferences. A larger spread may lose
        customers; lower targets need buyers to reduce holdings. These settings
        do not hedge existing holdings or fix a delayed price feed.
      </p>
      <p>
        Strategy references:{' '}
        <a
          className="text-accent underline"
          href="https://math.nyu.edu/~avellane/HighFrequencyTrading.pdf"
          target="_blank"
          rel="noreferrer"
        >
          Avellaneda & Stoikov
        </a>
        {' · '}
        <a
          className="text-accent underline"
          href="https://www.bis.org/publ/qtrpdf/r_qt1912g.htm"
          target="_blank"
          rel="noreferrer"
        >
          BIS on FX market making
        </a>
        . Stitch uses linear inventory adjustments and observed price ranges,
        not the full academic model.
      </p>
    </div>
  )
}

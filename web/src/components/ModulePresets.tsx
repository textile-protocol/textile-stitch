import { useId } from 'react'
import type { ModulesConfig } from '../modules'
import { percentage } from '../modulePresentation'
import {
  applyPreset,
  matchingPreset,
  modulePresets,
  presetConflict,
  presetSummary,
  type PresetModule,
} from '../modulePresets'
import { Disclosure } from './ModuleControls'

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
  const id = useId()
  const selected = matchingPreset(draft, module)
  const cngnUsdt =
    currency.toLowerCase() === 'cngn' && settlement.toLowerCase() === 'usdt'
  return (
    <div className="border-line-soft space-y-4 border-b p-5 sm:p-6">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h4 className="text-sm font-bold" id={id}>
          Start with a preset
        </h4>
        <span className="text-muted text-xs" role="status">
          {selected ? `${selected.name} settings` : 'Custom settings'}
        </span>
      </div>
      <div role="group" aria-labelledby={id} className="grid grid-cols-3 gap-2">
        {modulePresets.map((preset) => {
          const conflict = presetConflict(draft, module, preset)
          const active = selected?.id === preset.id
          return (
            <button
              key={preset.id}
              type="button"
              aria-pressed={active}
              aria-describedby={`${id}-${preset.id}`}
              disabled={!!conflict}
              onClick={() => onChange(applyPreset(draft, module, preset))}
              className={`min-w-0 rounded-xl border px-1 py-3 text-center transition focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent disabled:cursor-not-allowed disabled:opacity-50 sm:px-3 ${active ? 'bg-accent-tint border-accent' : 'border-line-soft hover:bg-canvas'}`}
            >
              <span className="block text-xs font-bold sm:text-sm">
                {preset.name}
              </span>
              <span className="text-muted mt-1 block text-xs">
                {module === 'inventory'
                  ? `${percentage(preset.inventory.target_bps)} target`
                  : `${percentage(preset.spreads.max_extra_bps)} cap`}
              </span>
              <span id={`${id}-${preset.id}`} className="sr-only">
                {presetSummary(preset, module)}.{' '}
                {conflict ?? preset.tradeoff[module]}
              </span>
            </button>
          )
        })}
      </div>
      <div className="text-sm" aria-live="polite" aria-atomic="true">
        <p>
          {selected?.tradeoff[module] ??
            'Choose a starting point, or keep your own settings below.'}
        </p>
        {selected && (
          <p className="text-muted mt-1 text-xs leading-relaxed">
            {presetSummary(selected, module)}
          </p>
        )}
      </div>
      {modulePresets.map((preset) => {
        const conflict = presetConflict(draft, module, preset)
        return conflict ? (
          <p key={preset.id} className="text-xs text-warning">
            {preset.name}: {conflict}
          </p>
        ) : null
      })}
      <p className="text-muted text-xs leading-relaxed">
        Changes this module’s draft only. Fine-tune below, then test on history
        before saving.
        {module === 'inventory'
          ? ` Your minimum margin stays at ${Number.isFinite(draft.inventory.spread_floor_bps) ? percentage(draft.inventory.spread_floor_bps) : 'your custom value'}.`
          : ' All presets favor trades toward your inventory target and use a 2-minute warmup.'}
      </p>
      <Disclosure title="How these presets were chosen">
        <div className="text-muted space-y-3 text-xs leading-relaxed">
          <p>
            Professional market makers shift quotes to manage inventory and
            widen margins as price risk rises. These are Textile starting points
            based on those principles, not a prediction of returns.
          </p>
          <p>
            {cngnUsdt ? 'Checked against' : 'The reference check used'} 1,440
            recorded cNGN/USDT prices over 6–7 Oct 2026 (UTC). Updates were
            typically a minute apart. The 95th-percentile price ranges were
            0.043% over 5 minutes, 0.079% over 15 minutes and 0.103% over 30
            minutes.
            {cngnUsdt
              ? ' This was one day with only five vault trades.'
              : ' These numbers are not a calibration for your corridor.'}
          </p>
          <p>
            Inventory targets are risk preferences. A larger spread may lose
            customers; lower targets need buyers to reduce holdings. These
            settings do not hedge existing holdings or fix a delayed price feed.
            Test several periods and include your costs.
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
            . Stitch uses linear inventory adjustments and observed price
            ranges, not the full academic model.
          </p>
        </div>
      </Disclosure>
    </div>
  )
}

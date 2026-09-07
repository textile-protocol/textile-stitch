// The body of the Spread step: the two spread fields, the quick picks and the
// worked example. No footer, so both wizard paths can put their own buttons
// under it and neither can drift from the other's wording.
//
// Spreads are per corridor. On a bot that already quotes something else, the
// numbers here apply to the new corridor alone.

import { SpreadField } from '../SettingsForm'
import { BPS_LIMIT, SPREAD_BANDS, SpreadExample, parseBps } from '../SpreadExample'
import type { Spread } from '../../types'

export interface Spreads {
  buy: Spread
  sell: Spread
}

// Same parser as the worked example, plus the backend's own bound: a buy
// spread at or above 10,000 bps prices the bid at zero and the write is
// refused. Both checks here so the operator finds out on this step.
export function spreadsOk(s: Spreads): boolean {
  const buy = parseBps(s.buy.value)
  return buy !== null && buy < BPS_LIMIT && parseBps(s.sell.value) !== null
}

export default function SpreadFields({
  spreads,
  symbols,
  onChange,
}: {
  spreads: Spreads
  /** The pair's symbols, when they are known. Null for a custom corridor. */
  symbols: { base: string; quote: string } | null
  onChange: (next: Spreads) => void
}) {
  return (
    <>
      <div className="mt-4 grid grid-cols-1 gap-4 sm:grid-cols-2">
        <SpreadField
          label="Buy spread"
          hint={
            symbols
              ? `How much less than the mid the bot pays for ${symbols.base}.`
              : 'How much less than the mid the bot pays.'
          }
          value={spreads.buy}
          disabled={false}
          onChange={(v) => onChange({ ...spreads, buy: v })}
        />
        <SpreadField
          label="Sell spread"
          hint={
            symbols
              ? `How much more than the mid the bot charges for ${symbols.base}.`
              : 'How much more than the mid the bot charges.'
          }
          value={spreads.sell}
          disabled={false}
          onChange={(v) => onChange({ ...spreads, sell: v })}
        />
      </div>
      {/* Quick picks: a word instead of a number, for an operator who doesn't
          think in bps. Each sets both sides to the same per-side value; the
          example below adds the two up. */}
      <div className="mt-3 flex flex-wrap items-center gap-2 text-xs text-muted">
        <span>Quick pick:</span>
        {SPREAD_BANDS.map((band) => {
          const active =
            parseBps(spreads.buy.value) === band.bps &&
            parseBps(spreads.sell.value) === band.bps
          return (
            <button
              key={band.name}
              type="button"
              onClick={() =>
                onChange({
                  buy: { kind: 'bps', value: String(band.bps) },
                  sell: { kind: 'bps', value: String(band.bps) },
                })
              }
              className={`rounded-full px-3 py-1 text-sm ${
                active ? 'bg-accent text-on-accent' : 'bg-hover text-muted'
              }`}
            >
              {band.name} · {band.bps} bps each side
            </button>
          )
        })}
      </div>
      <SpreadExample
        buy={spreads.buy}
        sell={spreads.sell}
        base={symbols?.base ?? null}
        quote={symbols?.quote ?? null}
      />
    </>
  )
}

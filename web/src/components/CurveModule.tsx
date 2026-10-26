import { useState } from 'react'
import {
  bpsOver,
  cheapestUntil,
  curveFor,
  curveRequirements,
  flatPrice,
  freshReading,
  shareAfter,
  takerPrice,
  type Curve,
  type CurveQuote,
  type CurveReading,
} from '../curve'
import { percentage } from '../modulePresentation'
import type { ModulesConfig, ModulesView } from '../modules'
import CurveChart from './CurveChart'
import { NumberControl, SecondarySettings } from './ModuleControls'
import { Banner, Card, Select } from './ui'

// The inventory curve's pieces of the module workspace: its editor fields,
// its Simulate content, its Overview card and its quote history. Everything
// it shows comes from the module view the vault modules use; the curve maths
// is `curve.ts`, pinned to the bot's.

const ngn = (n: number) =>
  n.toLocaleString(undefined, {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  })
const signedBps = (n: number) =>
  `${n >= 0 ? '+' : '−'}${Math.abs(n).toLocaleString(undefined, { maximumFractionDigits: 1 })} bps`
const share = (f: number) => percentage(Math.round(f * 10_000))
/** A threshold reads better whole: "84%", not "83.74%". */
const wholeShare = (f: number) => percentage(Math.round(f * 100) * 100)
const money = (n: number) =>
  n.toLocaleString(undefined, { maximumFractionDigits: 0 })

/** The corridor's one bps spread, or null when the two sides differ or
 * either is absolute: the curve then can't run, and says so. */
function equalSpreadBps(view: Pick<ModulesView, 'corridor_spreads'>) {
  const sides = [view.corridor_spreads.buy, view.corridor_spreads.sell].filter(
    (s) => s !== null
  )
  const first = sides[0]
  return first &&
    sides.every((s) => s.kind === 'bps' && s.value === first.value)
    ? first.value
    : null
}

/** The curve the bot would price with now, from its last reading and the
 * draft's offsets, or null without a reading to size it. */
function curveNow(
  view: ModulesView,
  knobs: ModulesConfig['curve'],
  reading: CurveReading | null
): Curve | null {
  const spread = equalSpreadBps(view)
  if (!reading || spread === null) return null
  return curveFor(
    { mid: reading.reference_mid, half_range_bps: reading.half_range_bps },
    spread + reading.fee_bps,
    knobs
  )
}

/** The editor: where the bot's prices sit across its holdings, the two
 * offsets and the shape, and what the curve needs on this bot. */
export function CurveFields({
  draft,
  onChange,
  view,
  now,
  currency,
  settlement,
  example,
}: {
  draft: ModulesConfig
  onChange: (config: ModulesConfig) => void
  view: ModulesView
  now: number
  currency: string
  settlement: string
  /** The Simulate example, while it is open: marked on the chart. */
  example: { share: number; sizeUsd: number } | null
}) {
  const c = draft.curve
  const set = (curve: Partial<ModulesConfig['curve']>) =>
    onChange({ ...draft, curve: { ...c, ...curve } })
  const reading = freshReading(view.status?.curve, now)
  const curve = curveNow(view, c, reading)
  const marker = example
    ? { share: example.share, label: `Example · ${share(example.share)}` }
    : reading
      ? { share: reading.share, label: `Now · ${share(reading.share)}` }
      : null
  return (
    <>
      {draft.mode === 'live' && c.enabled && (
        <Banner tone="warning">
          Live replaces the flat price on Swap quotes and on the levels Textile
          routes on. The bot then prices off the feed&apos;s centre before any
          manual adjustment, so Textile&apos;s hand-set lean no longer moves
          this bot&apos;s quotes.
        </Banner>
      )}
      {curve && reading ? (
        <CurveChart
          curve={curve}
          halfRangeBps={reading.half_range_bps}
          currency={currency}
          holder={view.vault ? 'vault' : 'wallet'}
          marker={marker}
          stretches={
            example && reading
              ? ([true, false] as const).map((botBuys) => ({
                  botBuys,
                  from: example.share,
                  to: shareAfter(
                    example.share,
                    example.sizeUsd,
                    reading.value,
                    botBuys
                  ),
                }))
              : []
          }
        />
      ) : (
        <p className="bg-hover text-muted rounded-lg p-4 text-sm">
          The chart draws once the running bot has read a reference book and its
          balances.
        </p>
      )}
      <NumberControl
        label="Start below the mid"
        unit="NGN"
        scale={1}
        max={1000}
        step={0.05}
        fractionDigits={2}
        sliderMax={5}
        value={c.start_below_mid}
        onChange={(start_below_mid) => set({ start_below_mid })}
        hint={`With no ${currency}, the bot sells ${settlement} this far under the reference mid, all-in. 0 sells at the mid. More wins flow that buys ${settlement}, at a subsidy.`}
      />
      <NumberControl
        label="End above the reference buy"
        unit="NGN"
        scale={1}
        max={1000}
        step={0.05}
        fractionDigits={2}
        sliderMax={5}
        value={c.end_above_buy}
        onChange={(end_above_buy) => set({ end_above_buy })}
        hint={`Full of ${currency}, the bot pays this far over the reference buy for ${settlement}, all-in. 0 matches what clearing ${currency} on the reference market would cost.`}
      />
      <NumberControl
        label="Shape"
        unit="×"
        scale={1}
        min={0.5}
        max={4}
        step={0.5}
        sliderMax={4}
        value={c.shape}
        onChange={(shape) => set({ shape })}
        hint="2 keeps quotes near the mid through normal holdings and leans hardest near full. Higher leans later; 1 leans evenly."
      />
      <SecondarySettings title="What the curve needs">
        {curveRequirements(
          { ...view, config: draft },
          currency,
          settlement,
          now
        ).map((r) => (
          <div key={r.label} className="flex gap-3 text-sm">
            <span
              aria-hidden
              className={`w-4 shrink-0 text-center font-bold ${r.ok ? 'text-success' : 'text-warning'}`}
            >
              {r.ok ? '✓' : '!'}
            </span>
            <div>
              <p className="font-bold">
                {r.label}
                <span className="sr-only">{r.ok ? ': met' : ': not met'}</span>
              </p>
              <p className="text-muted text-xs">{r.detail}</p>
            </div>
          </div>
        ))}
      </SecondarySettings>
    </>
  )
}

/** The Simulate drawer's content for the curve: one example trade, both
 * ways, flat against curved, across the holdings it moves. */
export function CurveExample({
  draft,
  view,
  now,
  currency,
  example,
  onExample,
}: {
  draft: ModulesConfig
  view: ModulesView
  now: number
  currency: string
  example: { share: number; sizeUsd: number }
  onExample: (example: { share: number; sizeUsd: number }) => void
}) {
  const reading = freshReading(view.status?.curve, now)
  const curve = curveNow(view, draft.curve, reading)
  if (!curve || !reading)
    return (
      <p className="text-muted text-sm">
        Simulating needs a reference book and balances from the running bot.
      </p>
    )
  const sides = ([true, false] as const).map((botBuys) => {
    const after = shareAfter(
      example.share,
      example.sizeUsd,
      reading.value,
      botBuys
    )
    const price = takerPrice(curve, botBuys, example.share, after)
    const flat = flatPrice(curve, botBuys)
    return { botBuys, after, price, flat, lean: bpsOver(price, flat) }
  })
  type Side = (typeof sides)[number]
  const rows: [string, (s: Side) => string, boolean][] = [
    ['Flat quote', (s) => ngn(s.flat), false],
    ['Curve lean', (s) => signedBps(s.lean), true],
    ['Quote', (s) => ngn(s.price), false],
    [`${currency} after`, (s) => share(s.after), false],
  ]
  return (
    <div className="space-y-6">
      <table className="w-full text-sm tabular-nums" aria-live="polite">
        <thead>
          <tr className="text-muted whitespace-nowrap text-xs">
            <th className="pb-2 text-left font-normal">
              <span className="sr-only">Step</span>
            </th>
            <th className="pb-2 text-right font-normal">Bot buys</th>
            <th className="pb-2 pl-3 text-right font-normal">Bot sells</th>
          </tr>
        </thead>
        <tbody>
          {rows.map(([row, cell, marked]) => (
            <tr
              key={row}
              className={
                row === 'Quote' ? 'border-line-soft border-t font-bold' : ''
              }
            >
              <th
                scope="row"
                className={`border-l-2 py-1.5 pl-2 pr-2 text-left ${marked ? 'border-accent font-bold' : 'border-transparent'} ${row === 'Quote' ? 'font-bold' : 'font-normal'}`}
              >
                {row}
              </th>
              {sides.map((s, i) => (
                <td
                  key={String(s.botBuys)}
                  className={`whitespace-nowrap py-1.5 text-right ${i ? 'pl-3' : ''}`}
                >
                  {cell(s)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
      <p className="text-muted text-xs leading-relaxed">
        NGN per USDT, all-in. Priced across the holdings this trade moves
        through, so splitting it into smaller quotes costs the same, and a round
        trip always costs the taker the spread.
      </p>
      <p className="text-muted text-xs leading-relaxed">
        {draft.mode === 'live'
          ? 'Live quotes and published levels use this price.'
          : 'Preview records this quote. Live quotes stay flat until you switch to Live.'}
      </p>
      {Math.abs(reading.share - example.share) > 1e-4 && (
        <button
          type="button"
          onClick={() => onExample({ ...example, share: reading.share })}
          className="text-left text-xs font-bold text-accent hover:underline"
        >
          Use the bot&apos;s current {currency} holdings
        </button>
      )}
      <NumberControl
        label="Trade size"
        unit="USDT"
        scale={1}
        max={10_000_000}
        step={1000}
        sliderMax={100_000}
        value={example.sizeUsd}
        onChange={(sizeUsd) => onExample({ ...example, sizeUsd })}
      />
      <div className="border-line-soft border-t pt-5">
        <NumberControl
          label={`Example ${currency} holdings`}
          value={Math.round(example.share * 10_000)}
          max={10_000}
          sliderMax={10_000}
          onChange={(bps) => onExample({ ...example, share: bps / 10_000 })}
        />
      </div>
    </div>
  )
}

/** What the curve sees right now: holdings, where they sit against the
 * reference buy, and the small-trade quotes it gives (or would, in Preview). */
export function CurveNow({
  view,
  now,
  currency,
  settlement,
}: {
  view: ModulesView
  now: number
  currency: string
  settlement: string
}) {
  const config = view.config
  const reading = freshReading(view.status?.curve, now)
  const curve = curveNow(view, config.curve, reading)
  const f = reading?.share ?? null
  const until =
    curve && reading ? cheapestUntil(curve, reading.half_range_bps) : null
  const preview = config.mode === 'shadow'
  const quotes = (
    [
      [`Bot buys ${currency}`, true],
      [`Bot sells ${currency}`, false],
    ] as const
  ).map(([label, botBuys]) => {
    const price = curve && f !== null ? takerPrice(curve, botBuys, f, f) : null
    return {
      label,
      price,
      vsMid: price && curve ? bpsOver(price, curve.mid) : null,
    }
  })
  const quotes_ = view.status?.curve?.quotes ?? []
  const flat = quotes_.filter((q) => q.flat_reason !== null).length
  return (
    <div className="grid gap-6 sm:grid-cols-2">
      <div>
        <p className="text-muted text-sm">
          {currency} · share of {view.vault ? 'vault' : 'wallet'} value
        </p>
        <p className="mb-4 mt-1 text-3xl font-bold tabular-nums">
          {f === null ? '—' : share(f)}
        </p>
        {f !== null && until !== null ? (
          <>
            <div
              className="space-y-2"
              role="img"
              aria-label={`${currency} holdings ${share(f)}; cheapest ${settlement} until ${wholeShare(until)}`}
            >
              <div className="bg-active relative h-3 rounded-full">
                <div
                  className="h-full rounded-full bg-accent"
                  style={{ width: `${f * 100}%` }}
                />
                <span
                  className="border-muted absolute -top-1 h-5 border-l-2 border-dashed"
                  style={{ left: `${until * 100}%` }}
                />
              </div>
              <div className="text-muted flex justify-between text-xs">
                <span>0%</span>
                <span>
                  Cheapest {settlement} until {wholeShare(until)}
                </span>
              </div>
            </div>
            <p className="text-muted mt-3 text-sm">
              {f >= until
                ? `Past ${wholeShare(until)}, the reference market sells ${settlement} cheaper than this bot: it is leaning hard to shed ${currency}.`
                : `Below ${wholeShare(until)}, this bot sells ${settlement} cheaper than the reference buy.`}
            </p>
          </>
        ) : (
          <p className="text-muted text-sm">
            {view.running
              ? 'Waiting for a reference book and balances.'
              : 'Start the bot to read its holdings.'}
          </p>
        )}
      </div>
      <div className="sm:border-line-soft sm:border-l sm:pl-6">
        <p className="text-muted text-sm">
          {preview
            ? 'Preview quotes · small trade'
            : 'Curve quotes · small trade'}
        </p>
        <div className="my-4 grid grid-cols-2 gap-4">
          {quotes.map((q) => (
            <div key={q.label}>
              <p className="text-muted text-xs">{q.label}</p>
              <p className="mt-1 text-2xl font-bold tabular-nums">
                {q.price === null ? '—' : ngn(q.price)}
              </p>
              {q.vsMid !== null && (
                <p className="text-muted text-xs tabular-nums">
                  mid {signedBps(q.vsMid)}
                </p>
              )}
            </div>
          ))}
        </div>
        <p className="text-muted text-sm">
          NGN per USDT, venue fee included, off the reference mid. Larger trades
          pay the average across the holdings they move.
        </p>
        {quotes_.length > 0 && flat / quotes_.length >= 0.1 && (
          <p className="bg-warning-bg mt-4 rounded-lg p-3 text-sm text-warning">
            {flat} of the last {quotes_.length} quotes went out flat. The curve
            doesn&apos;t lean on those; Decisions says why.
          </p>
        )}
      </div>
    </div>
  )
}

/** Every Swap request the curve priced or answered flat, newest first. */
export function CurveQuotes({
  quotes,
  currency,
  settlement,
}: {
  quotes: CurveQuote[]
  currency: string
  settlement: string
}) {
  const [filter, setFilter] = useState<'all' | 'flat'>('all')
  const rows = [...quotes]
    .reverse()
    .filter((q) => filter === 'all' || q.flat_reason !== null)
  return (
    <Card title="Inventory curve quotes">
      <p className="text-muted mb-3 text-sm">
        Every Swap request the curve priced or answered flat, newest first. In
        Preview these are what it would have quoted. The lean is against the
        flat quote in NGN per USDT: positive means more naira per dollar.
      </p>
      <Select
        aria-label="Filter curve quotes"
        value={filter}
        onChange={(e) => setFilter(e.target.value as 'all' | 'flat')}
      >
        <option value="all">All quotes</option>
        <option value="flat">Answered flat</option>
      </Select>
      {rows.length === 0 && (
        <p className="text-muted py-6">No recorded quotes.</p>
      )}
      <ul className="mt-3 space-y-2">
        {rows.map((q, i) => (
          <li
            key={`${q.at}:${i}`}
            className="border-line-soft flex flex-wrap items-baseline gap-x-3 gap-y-1 rounded-lg border p-3 text-sm tabular-nums"
          >
            <span className="text-muted w-40 shrink-0 text-xs">
              {new Date(q.at * 1000).toLocaleString()}
            </span>
            <span className="font-bold">
              Bot {q.bot_buys ? 'buys' : 'sells'} {currency}
            </span>
            <span>
              {money(q.size)} {settlement}
            </span>
            {q.share_before !== null && q.share_after !== null && (
              <span className="text-muted">
                {share(q.share_before)} → {share(q.share_after)}
              </span>
            )}
            <span className="ml-auto">
              {q.flat_reason !== null ? (
                <span className="text-warning">Flat · {q.flat_reason}</span>
              ) : (
                <>
                  <span className="text-muted">vs flat </span>
                  <strong>{signedBps(q.lean_bps ?? 0)}</strong>
                  {!q.live && <span className="text-muted"> · preview</span>}
                </>
              )}
            </span>
          </li>
        ))}
      </ul>
    </Card>
  )
}

import { useId, useState } from 'react'
import {
  exampleSide,
  feedPrice,
  flatPrice,
  freshReading,
  readsUnadjustedPrice,
  referenceTooFar,
  shareAfter,
  withLift,
} from '../curve'
import { draftUnwindCapLimit, percentage } from '../modulePresentation'
import type { ModulesConfig, ModulesView } from '../modules'
import {
  draftUnwindLift,
  flatOrderNgn,
  flatUnwindSide,
  freshUnwindReading,
  heldLift,
  liftLimitNgn,
  minSpreadBps,
  unwindRequirements,
  unwindWeight,
  type UnwindLift,
  type UnwindQuote,
  type UnwindReading,
} from '../unwind'
import { ticks, useWidth } from './CurveChart'
import { curveNow } from './CurveModule'
import {
  ExampleTable,
  ModuleSwitch,
  NumberControl,
  SecondarySettings,
  type ExampleRow,
} from './ModuleControls'
import { Banner, Card, Select } from './ui'

// Profit unwind's pieces of the module workspace: its editor fields, its
// Simulate content, its Overview body and its quote history. Everything it shows comes from the
// module view; the maths is `unwind.ts`, pinned to the bot's.

const priceText = (n: number) =>
  n.toLocaleString(undefined, {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  })
const liftText = (n: number) => `+${priceText(n)}`
const signedBps = (n: number) =>
  `${n >= 0 ? '+' : '−'}${Math.abs(n).toLocaleString(undefined, { maximumFractionDigits: 1 })} bps`
const share = (f: number) => percentage(Math.round(f * 10_000))
const money = (n: number) =>
  n.toLocaleString(undefined, { maximumFractionDigits: 0 })
const ago = (secs: number) =>
  secs < 90
    ? `${Math.max(0, Math.round(secs))}s ago`
    : `${Math.round(secs / 60)} min ago`

/** The lift the bot would quote with now for the draft's knobs: sized off
 * its frozen cost, held to the round-trip limit at the corridor spread.
 * Null while there's no lift: off, no cost yet, or nothing to give back. */
function liftNow(
  view: ModulesView,
  draft: ModulesConfig,
  reading: UnwindReading | null,
  now: number
): UnwindLift | null {
  const lift = draftUnwindLift(draft.profit_unwind, reading)
  if (!lift) return null
  const limit = draftUnwindCapLimit(view, draft, now)
  return heldLift(lift, limit ?? Infinity)
}

/** The editor: the lift across the holdings, whether it quotes, the four
 * knobs and what it needs on this bot. */
export function ProfitUnwindFields({
  draft,
  onChange,
  view,
  now,
  currency,
  settlement,
}: {
  draft: ModulesConfig
  onChange: (config: ModulesConfig) => void
  view: ModulesView
  now: number
  currency: string
  settlement: string
}) {
  const u = draft.profit_unwind
  const set = (patch: Partial<typeof u>) =>
    onChange({ ...draft, profit_unwind: { ...u, ...patch } })
  const reading = freshUnwindReading(view.status?.profit_unwind, now)
  const capLimit = draftUnwindCapLimit(view, draft, now)
  const lift = liftNow(view, draft, reading, now)
  const most = Math.min(Math.max(0, u.cap_ngn), capLimit ?? Infinity)
  return (
    <>
      {draft.mode === 'live' && u.enabled && u.live && (
        <Banner tone="warning">
          Live adds the lift to Swap quotes and to the levels Textile routes on:
          on top of the inventory curve when it quotes, on the flat price
          otherwise.
        </Banner>
      )}
      <UnwindChart
        fullNgn={lift?.fullNgn ?? 0}
        mostNgn={Number.isFinite(most) ? most : null}
        startPct={u.start_pct}
        marker={reading?.share ?? null}
        currency={currency}
        settlement={settlement}
      />
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0">
          <p className="text-sm font-bold">Quote with the lift</p>
          <p className="text-muted mt-1 max-w-md text-xs">
            {u.live
              ? draft.mode === 'live'
                ? 'Swap quotes and published levels carry the lift.'
                : 'Carries the lift once the quote policy is Live. Until then the bot records what it would quote.'
              : 'Preview: the bot tracks the cost, logs what it would quote and quotes without the lift.'}
          </p>
        </div>
        <ModuleSwitch
          label="Quote with the lift"
          checked={u.live}
          onChange={(live) => set({ live })}
        />
      </div>
      <UnwindNow
        reading={reading}
        lift={lift}
        on={u.enabled}
        currency={currency}
        settlement={settlement}
        now={now}
      />
      <NumberControl
        label="Share of gain given back"
        unit="%"
        scale={0.01}
        max={1}
        step={0.05}
        sliderMax={1}
        value={u.share}
        onChange={(share) => set({ share })}
        hint={`Of each ${currency} per ${settlement} the holdings gained, how much goes to takers as a better price. The rest is kept.`}
      />
      <NumberControl
        label="Cap"
        unit={`${currency}/${settlement}`}
        scale={1}
        max={1000}
        step={0.05}
        fractionDigits={2}
        sliderMax={5}
        value={u.cap_ngn}
        onChange={(cap_ngn) => set({ cap_ngn })}
        hint={
          capLimit === null
            ? `The most both prices ever move, per ${settlement}.`
            : `The most both prices ever move, per ${settlement}. At today's spread and start it can be at most ${priceText(capLimit)}, so a taker can't profit from the lift falling between trades.`
        }
      />
      <NumberControl
        label="Ignore gains under"
        unit={`${currency}/${settlement}`}
        scale={1}
        max={1000}
        step={0.05}
        fractionDigits={2}
        sliderMax={3}
        value={u.min_gain_ngn}
        onChange={(min_gain_ngn) => set({ min_gain_ngn })}
        hint="Smaller gains move nothing, so feed noise doesn't move the price."
      />
      <NumberControl
        label="Start at"
        unit="%"
        scale={1}
        max={99}
        step={5}
        sliderMax={99}
        value={u.start_pct}
        onChange={(start_pct) => set({ start_pct })}
        hint={`Below this ${currency} share nothing moves. Above it the lift grows evenly to full at 100%.`}
      />
      <SecondarySettings title="What profit unwind needs">
        {unwindRequirements(
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

/** The numbers behind the lift: the frozen cost, the feed, the gain and
 * what the draft's knobs give at full and at today's share. */
function UnwindNow({
  reading,
  lift,
  on,
  currency,
  settlement,
  now,
}: {
  reading: UnwindReading | null
  lift: UnwindLift | null
  on: boolean
  currency: string
  settlement: string
  now: number
}) {
  if (!reading)
    return (
      <p className="bg-hover text-muted rounded-lg p-4 text-sm">
        {on
          ? `The cost of the ${currency} held shows once the running bot has read its balances with profit unwind on.`
          : `Off. The bot tracks what its ${currency} cost once profit unwind is on.`}
      </p>
    )
  const full = lift?.fullNgn ?? 0
  const rows: [string, string][] = [
    ['Average cost', priceText(reading.cost)],
    ['Feed', priceText(reading.feed_mid)],
    [
      'Gain per dollar',
      `${reading.gain >= 0 ? '+' : '−'}${priceText(Math.abs(reading.gain))}`,
    ],
    ['Lift at 100%', liftText(full)],
    ...(reading.share === null
      ? []
      : ([
          [
            `Lift at ${share(reading.share)} ${currency}`,
            liftText(full * unwindWeight(reading.share, lift?.startPct ?? 0)),
          ],
        ] as [string, string][])),
  ]
  return (
    <div className="border-line-soft rounded-xl border p-4">
      <dl className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
        {rows.map(([label, value]) => (
          <div key={label} className="contents">
            <dt className="text-muted">{label}</dt>
            <dd className="text-right font-bold tabular-nums">{value}</dd>
          </div>
        ))}
      </dl>
      <p className="text-muted mt-3 text-xs leading-relaxed">
        {currency} per {settlement}. The cost was frozen{' '}
        {ago(now - reading.snapshot_at)}; the gain follows every price. Sells
        never lift past the average cost.
      </p>
    </div>
  )
}

const H = 170
const PAD = { top: 22, right: 16, bottom: 30, left: 48 }
const PLOT_H = H - PAD.top - PAD.bottom

/** How far both prices lift across the corridor-currency share: nothing up
 * to the start, then evenly to the full lift at 100%. The dashed line is the
 * most it can lift, the cap held to the round-trip limit. */
function UnwindChart({
  fullNgn,
  mostNgn,
  startPct,
  marker,
  currency,
  settlement,
}: {
  fullNgn: number
  mostNgn: number | null
  startPct: number
  /** The bot's share now, 0..1, when it has one. */
  marker: number | null
  currency: string
  settlement: string
}) {
  const titleId = useId()
  const [box, W] = useWidth(560)
  const PLOT_W = W - PAD.left - PAD.right
  // Drawn only where it says something: above today's lift.
  const most = mostNgn !== null && mostNgn > fullNgn + 1e-9 ? mostNgn : null
  const top = Math.max(fullNgn, most ?? 0, 0.5) * 1.15
  const x = (f: number) => PAD.left + f * PLOT_W
  const y = (v: number) => PAD.top + (1 - v / top) * PLOT_H
  const start = Math.min(0.99, Math.max(0, startPct / 100))
  const line = (full: number) =>
    `M${x(0)},${y(0)}L${x(start)},${y(0)}L${x(1)},${y(full)}`
  const at = marker === null ? null : fullNgn * unwindWeight(marker, startPct)
  return (
    <figure className="space-y-3" aria-labelledby={titleId}>
      <figcaption className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
        <span id={titleId} className="text-sm font-bold">
          Lift across your {currency} share
        </span>
        <span className="text-muted flex gap-4 text-xs">
          <span className="inline-flex items-center gap-1.5">
            <span aria-hidden className="h-0.5 w-4 rounded-full bg-accent" />
            Now
          </span>
          {most !== null && (
            <span className="inline-flex items-center gap-1.5">
              <span
                aria-hidden
                className="w-4 border-t-2 border-dashed border-current"
              />
              Most it can lift
            </span>
          )}
        </span>
      </figcaption>
      <div ref={box}>
        <svg
          viewBox={`0 0 ${W} ${H}`}
          width={W}
          height={H}
          className="text-muted block overflow-visible"
          role="img"
          aria-label={`No lift up to ${Math.round(start * 100)}% ${currency}, then rising to ${liftText(fullNgn)} ${currency} per ${settlement} at 100%.`}
        >
          {ticks(0, top).map((t) => (
            <g key={t}>
              <line
                x1={PAD.left}
                x2={W - PAD.right}
                y1={y(t)}
                y2={y(t)}
                stroke="var(--tx-border-tertiary)"
              />
              <text
                x={PAD.left - 8}
                y={y(t)}
                dy="0.32em"
                textAnchor="end"
                className="fill-current text-[11px] tabular-nums"
              >
                {priceText(t)}
              </text>
            </g>
          ))}
          {[0, 0.25, 0.5, 0.75, 1].map((f) => (
            <text
              key={f}
              x={x(f)}
              y={H - 10}
              textAnchor="middle"
              className="fill-current text-[11px] tabular-nums"
            >
              {f * 100}%
            </text>
          ))}
          {start > 0 && (
            <g>
              <rect
                x={x(0)}
                width={x(start) - x(0)}
                y={PAD.top}
                height={PLOT_H}
                fill="var(--tx-text-primary)"
                opacity={0.04}
              />
              <text
                x={x(start / 2)}
                y={PAD.top + 14}
                textAnchor="middle"
                className="fill-current text-[10px]"
              >
                No lift
              </text>
            </g>
          )}
          {most !== null && (
            <path
              d={line(most)}
              fill="none"
              stroke="var(--tx-border-secondary)"
              strokeWidth={1.5}
              strokeDasharray="4 4"
            />
          )}
          <path
            d={line(fullNgn)}
            fill="none"
            stroke="var(--tx-accent)"
            strokeWidth={2}
            strokeLinejoin="round"
            strokeLinecap="round"
          />
          {marker !== null && at !== null && (
            <g>
              <text
                x={x(marker)}
                y={PAD.top - 8}
                textAnchor={
                  marker > 0.85 ? 'end' : marker < 0.15 ? 'start' : 'middle'
                }
                className="fill-current text-[10px] font-bold"
              >
                Now · {share(marker)}
              </text>
              <circle
                cx={x(marker)}
                cy={y(at)}
                r={4}
                fill="var(--tx-accent)"
                stroke="var(--tx-bg-secondary)"
                strokeWidth={2}
              />
            </g>
          )}
        </svg>
      </div>
      <p className="text-muted text-xs leading-relaxed">
        {currency} per {settlement} added to both prices: takers bringing{' '}
        {settlement} get more {currency}, and the bot asks more {currency} for
        the {settlement} it pays. A quote pays the average lift across the
        holdings it moves, so a round trip still costs the taker the spread.
      </p>
    </figure>
  )
}

/** The Overview: the lift now, across the holdings, and the cost it is
 * sized on. */
export function ProfitUnwindNow({
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
  const reading = freshUnwindReading(view.status?.profit_unwind, now)
  const lift = liftNow(view, view.config, reading, now)
  const full = lift?.fullNgn ?? 0
  const f = reading?.share ?? null
  const start = view.config.profit_unwind.start_pct / 100
  return (
    <div className="grid gap-6 sm:grid-cols-2">
      <div>
        <p className="text-muted text-sm">
          Lift now · {currency} per {settlement}
        </p>
        <p className="mb-1 mt-1 text-3xl font-bold tabular-nums">
          {reading && f !== null
            ? liftText(full * unwindWeight(f, lift?.startPct ?? 0))
            : '—'}
        </p>
        <p className="text-muted mb-4 text-xs tabular-nums">
          {reading ? `${liftText(full)} at 100% ${currency}` : ' '}
        </p>
        {f !== null ? (
          <div
            className="space-y-2"
            role="img"
            aria-label={`${currency} holdings ${share(f)}; lifts above ${share(start)}`}
          >
            <div className="bg-active relative h-3 rounded-full">
              <div
                className="h-full rounded-full bg-accent"
                style={{ width: `${f * 100}%` }}
              />
              <span
                className="border-muted absolute -top-1 h-5 border-l-2 border-dashed"
                style={{ left: `${start * 100}%` }}
              />
            </div>
            <div className="text-muted flex justify-between text-xs">
              <span>
                {share(f)} {currency}
              </span>
              <span>Lifts above {share(start)}</span>
            </div>
          </div>
        ) : (
          <p className="text-muted text-sm">
            {view.running
              ? 'Waiting for a price and balances.'
              : 'Start the bot to read its holdings.'}
          </p>
        )}
      </div>
      <div className="sm:border-line-soft sm:border-l sm:pl-6">
        <p className="text-muted text-sm">Cost basis</p>
        {reading ? (
          <>
            <dl className="my-4 grid grid-cols-2 gap-4">
              {(
                [
                  ['Average cost', priceText(reading.cost)],
                  ['Feed', priceText(reading.feed_mid)],
                  [
                    'Gain per dollar',
                    `${reading.gain >= 0 ? '+' : '−'}${priceText(Math.abs(reading.gain))}`,
                  ],
                  ['Cost frozen', ago(now - reading.snapshot_at)],
                ] as const
              ).map(([label, value]) => (
                <div key={label}>
                  <dt className="text-muted text-xs">{label}</dt>
                  <dd className="mt-1 text-xl font-bold tabular-nums">
                    {value}
                  </dd>
                </div>
              ))}
            </dl>
            <p className="text-muted text-sm">
              {currency} coming in is marked at the feed; {currency} going out
              leaves at the average cost. So takers can only pull the gain down,
              never up.
            </p>
          </>
        ) : (
          <p className="text-muted mt-4 text-sm">
            Tracked from the first balance reading with profit unwind on. It
            starts at the feed, so nothing lifts until a gain builds.
          </p>
        )}
      </div>
    </div>
  )
}

/** One side of profit unwind's example: the price it lifts, the lift and
 * what goes out, all-in NGN per USDT. Null where a side doesn't quote. */
interface UnwindSide {
  botBuys: boolean
  after: number
  base: number | null
  lift: number | null
  price: number | null
  atCost: boolean
}

/** The Simulate drawer's content for profit unwind: one example trade, both
 * ways, on the price it would lift: a Live curve's when the draft runs one
 * the bot can price, the flat price otherwise. */
export function UnwindExample({
  draft,
  view,
  now,
  currency,
  settlement,
  example,
  onExample,
}: {
  draft: ModulesConfig
  view: ModulesView
  now: number
  currency: string
  settlement: string
  example: { share: number; sizeUsd: number }
  onExample: (example: { share: number; sizeUsd: number }) => void
}) {
  const reading = freshUnwindReading(view.status?.profit_unwind, now)
  const valueUsd = reading?.value ?? null
  if (!reading || valueUsd === null || reading.share === null)
    return (
      <p className="text-muted text-sm">
        Simulating needs balances and a cost from the running bot.
      </p>
    )
  const u = draft.profit_unwind
  const raw = draftUnwindLift(u, reading)
  // Like the curve's, the example shows what Live would quote. The curve
  // quotes when it can price: a reference book within 1% of the feed.
  // Otherwise the flat price goes out and the lift goes on that.
  const curveReading = freshReading(view.status?.curve, now)
  const curveOn = draft.curve.enabled
  const plain = curveOn ? curveNow(view, draft, curveReading) : null
  const running = view.status?.config ?? view.config
  const curveFeed =
    curveReading &&
    readsUnadjustedPrice(draft) === readsUnadjustedPrice(running)
      ? feedPrice(curveReading)
      : null
  const onCurve =
    plain !== null &&
    curveReading !== null &&
    !(curveFeed !== null && referenceTooFar(curveReading, curveFeed))
  const limitSpread = minSpreadBps(view)
  const flatLift =
    raw && limitSpread !== null
      ? heldLift(raw, liftLimitNgn(limitSpread, reading.feed_mid, u.start_pct))
      : null
  const curveLift = raw && plain ? (withLift(plain, raw).lift ?? null) : null
  const held = onCurve ? curveLift : flatLift
  const sides: UnwindSide[] = ([true, false] as const).map((botBuys) => {
    const after = shareAfter(example.share, example.sizeUsd, valueUsd, botBuys)
    if (onCurve && plain) {
      const s = exampleSide({
        plain,
        lifted: raw ? withLift(plain, raw) : null,
        unwindLive: u.live,
        feed: null,
        flatOnly: false,
        share: example.share,
        sizeUsd: example.sizeUsd,
        valueUsd,
        botBuys,
      })
      return {
        botBuys,
        after,
        base: s.curved,
        lift: s.unwind,
        price: s.price,
        atCost: s.atCost,
      }
    }
    const spread = view.corridor_spreads[botBuys ? 'buy' : 'sell']
    if (spread?.kind !== 'bps')
      return {
        botBuys,
        after,
        base: null,
        lift: null,
        price: null,
        atCost: false,
      }
    const feed = reading.feed_mid
    const base = flatPrice(feed, spread.value + reading.fee_bps, botBuys)
    const s = flatUnwindSide({
      base,
      order: flatOrderNgn(feed, spread.value, botBuys),
      lift: flatLift,
      live: u.live,
      midNgn: feed,
      share: example.share,
      after,
      botBuys,
    })
    return {
      botBuys,
      after,
      base,
      lift: s.lift,
      price: s.price,
      atCost: s.atCost,
    }
  })
  const text = (n: number | null) => (n === null ? '—' : priceText(n))
  const rows: ExampleRow<UnwindSide>[] = [
    {
      label: onCurve ? 'Curve quote' : 'Flat quote',
      cell: (s) => text(s.base),
    },
    {
      label: u.live ? 'Profit unwind' : 'Profit unwind · preview',
      cell: (s) =>
        s.lift === null
          ? '—'
          : `${signedBps(s.lift)}${s.atCost ? ' · at cost' : ''}`,
      marked: true,
      muted: !u.live,
    },
    { label: 'Quote', cell: (s) => text(s.price), total: true },
    { label: `${currency} after`, cell: (s) => share(s.after) },
  ]
  return (
    <div className="space-y-6">
      <ExampleTable sides={sides} rows={rows} />
      <p className="text-muted text-xs leading-relaxed">
        {currency} per {settlement}, all-in.{' '}
        {onCurve
          ? 'Profit unwind lifts the inventory curve’s price here: the curve is on. '
          : curveOn
            ? `The curve quotes flat right now, so the lift goes on the flat price around the feed, ${priceText(reading.feed_mid)}. `
            : `Flat is spread and venue fee around the feed price the bot quotes on, ${priceText(reading.feed_mid)}. `}
        Priced across the holdings the trade moves through, so a round trip
        still costs the taker the spread.
      </p>
      <p className="text-muted text-xs leading-relaxed">
        {!raw || !held
          ? `Nothing to give back right now: the ${priceText(reading.cost)} average cost isn't far enough above the feed.`
          : u.live
            ? `Both prices lift by up to ${priceText(held.fullNgn)} at full ${currency}, less below that. When the bot sells ${currency} it stops at the ${priceText(reading.cost)} average cost.`
            : `Preview: the quote leaves the lift out. Quoting with it would add this row to both prices, up to ${priceText(held.fullNgn)} at full ${currency}.`}
        {raw &&
          held &&
          held.fullNgn < raw.fullNgn - 1e-9 &&
          ` The ${priceText(raw.fullNgn)} it would give is held to that by the round-trip limit at this spread.`}{' '}
        {draft.mode === 'live'
          ? u.live
            ? 'Live quotes and published levels use this price.'
            : ''
          : 'Live quotes carry no lift until the quote policy is Live.'}
      </p>
      {Math.abs(reading.share - example.share) > 1e-4 && (
        <button
          type="button"
          onClick={() => onExample({ ...example, share: reading.share ?? 0 })}
          className="text-left text-xs font-bold text-accent hover:underline"
        >
          Use the bot&apos;s current {currency} holdings
        </button>
      )}
      <NumberControl
        label="Trade size"
        unit={settlement}
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

/** Every Swap request profit unwind lifted or previewed, newest first. */
export function UnwindQuotes({
  quotes,
  currency,
  settlement,
}: {
  quotes: UnwindQuote[]
  currency: string
  settlement: string
}) {
  const [filter, setFilter] = useState<'all' | 'live'>('all')
  const rows = [...quotes].reverse().filter((q) => filter === 'all' || q.live)
  return (
    <Card title="Profit unwind quotes">
      <p className="text-muted mb-3 text-sm">
        Every Swap request profit unwind lifted, newest first. In preview these
        are what it would have quoted. The lift is against the price it lifted
        (the curve&apos;s or the flat one) in {currency} per {settlement}:
        positive means more {currency} per {settlement}.
      </p>
      <Select
        aria-label="Filter profit unwind quotes"
        value={filter}
        onChange={(e) => setFilter(e.target.value as 'all' | 'live')}
      >
        <option value="all">All quotes</option>
        <option value="live">Quoted with the lift</option>
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
            <span className="text-muted">
              {share(q.share_before)} → {share(q.share_after)}
            </span>
            <span className="ml-auto">
              <span className="text-muted">lift </span>
              <strong>{signedBps(q.lift_bps)}</strong>
              {!q.live && <span className="text-muted"> · preview</span>}
            </span>
          </li>
        ))}
      </ul>
    </Card>
  )
}

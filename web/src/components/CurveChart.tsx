import { useEffect, useId, useRef, useState, type PointerEvent } from 'react'
import { type Curve, takerPrice } from '../curve'

const H = 230
const PAD = { top: 22, right: 16, bottom: 30, left: 56 }
const PLOT_H = H - PAD.top - PAD.bottom
const STEPS = 60

/** The element's width in CSS pixels, so the chart draws at 1:1 and its text
 * stays the size it's set at however narrow the column gets. */
export function useWidth(fallback: number) {
  const ref = useRef<HTMLDivElement>(null)
  const [width, setWidth] = useState(fallback)
  useEffect(() => {
    const el = ref.current
    if (!el) return
    const observer = new ResizeObserver(([entry]) => {
      if (entry) setWidth(Math.max(240, entry.contentRect.width))
    })
    observer.observe(el)
    return () => observer.disconnect()
  }, [])
  return [ref, width] as const
}

const priceText = (n: number) =>
  n.toLocaleString(undefined, {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  })

/** Clean y ticks: three or four round values inside [lo, hi]. */
export function ticks(lo: number, hi: number): number[] {
  const raw = (hi - lo) / 3
  const mag = 10 ** Math.floor(Math.log10(raw))
  const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((s) => s >= raw)!
  const first = Math.ceil(lo / step) * step
  return Array.from({ length: 5 }, (_, i) => first + i * step).filter(
    (t) => t <= hi
  )
}

/**
 * Where the bot's all-in prices sit across its naira share: what it pays in
 * naira for a dollar when it buys cNGN (accent) and takes when it sells
 * (ink), against the reference mid and buy. Point prices; a quote pays the
 * average across the stretch it moves, which the shaded band marks for the
 * example trade.
 */
export default function CurveChart({
  curve,
  halfRangeBps,
  currency,
  settlement,
  holder,
  marker,
  stretches,
}: {
  curve: Curve
  halfRangeBps: number
  currency: string
  settlement: string
  /** What holds the inventory: the bot's wallet, or its vault. */
  holder: 'wallet' | 'vault'
  /** The share to mark, 0..1, and what it is: the wallet now, or the example. */
  marker: { share: number; label: string } | null
  /** The example trade's stretch of share on each side, when there is one. */
  stretches: { botBuys: boolean; from: number; to: number }[]
}) {
  const titleId = useId()
  const [box, W] = useWidth(560)
  const PLOT_W = W - PAD.left - PAD.right
  const [hover, setHover] = useState<number | null>(null)
  const buyRef = curve.mid * (1 + halfRangeBps / 10_000)
  const at = (botBuys: boolean, f: number) => takerPrice(curve, botBuys, f, f)
  const ends = [
    at(true, 0),
    at(true, 1),
    at(false, 0),
    at(false, 1),
    curve.mid,
    buyRef,
  ]
  const span = Math.max(...ends) - Math.min(...ends)
  const lo = Math.min(...ends) - span * 0.12
  const hi = Math.max(...ends) + span * 0.12
  const x = (f: number) => PAD.left + f * PLOT_W
  const y = (p: number) => PAD.top + (1 - (p - lo) / (hi - lo)) * PLOT_H
  const path = (botBuys: boolean) =>
    Array.from({ length: STEPS + 1 }, (_, i) => i / STEPS)
      .map(
        (f, i) =>
          `${i ? 'L' : 'M'}${x(f).toFixed(1)},${y(at(botBuys, f)).toFixed(1)}`
      )
      .join('')
  const series = [
    {
      key: 'buys',
      botBuys: true,
      label: `Bot buys ${currency}`,
      color: 'var(--tx-accent)',
    },
    {
      key: 'sells',
      botBuys: false,
      label: `Bot sells ${currency}`,
      color: 'var(--tx-text-primary)',
    },
  ] as const
  // Each label sits at the end its line is clear of: the curves start at the
  // mid on the left and finish at the buy on the right.
  const references = [
    { label: 'Reference mid', price: curve.mid, left: false },
    { label: 'Reference buy', price: buyRef, left: true },
  ]

  function track(e: PointerEvent<SVGRectElement>) {
    const box = e.currentTarget.getBoundingClientRect()
    const f = (e.clientX - box.left) / box.width
    setHover(Math.min(1, Math.max(0, f)))
  }

  return (
    <figure className="space-y-3" aria-labelledby={titleId}>
      <figcaption className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
        <span id={titleId} className="text-sm font-bold">
          All-in price across your {currency} share
        </span>
        <span className="text-muted flex gap-4 text-xs">
          {series.map((s) => (
            <span key={s.key} className="inline-flex items-center gap-1.5">
              <span
                aria-hidden
                className="h-0.5 w-4 rounded-full"
                style={{ background: s.color }}
              />
              {s.label}
            </span>
          ))}
        </span>
      </figcaption>
      <div ref={box} className="relative">
        <svg
          viewBox={`0 0 ${W} ${H}`}
          width={W}
          height={H}
          className="text-muted block overflow-visible"
          role="img"
          aria-label={`At no ${currency} the bot buys at ${priceText(at(true, 0))} and sells at ${priceText(at(false, 0))} ${currency} per ${settlement}; full of ${currency} it buys at ${priceText(at(true, 1))} and sells at ${priceText(at(false, 1))}.`}
        >
          {ticks(lo, hi).map((t) => (
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
          {stretches.map((st) => (
            <rect
              key={String(st.botBuys)}
              x={x(Math.min(st.from, st.to))}
              width={Math.max(1.5, Math.abs(x(st.to) - x(st.from)))}
              y={PAD.top}
              height={PLOT_H}
              fill={st.botBuys ? 'var(--tx-accent)' : 'var(--tx-text-primary)'}
              opacity={0.08}
            />
          ))}
          {references.map((r) => (
            <g key={r.label}>
              <line
                x1={PAD.left}
                x2={W - PAD.right}
                y1={y(r.price)}
                y2={y(r.price)}
                stroke="var(--tx-border-secondary)"
              />
              <text
                x={r.left ? PAD.left + 4 : W - PAD.right}
                y={y(r.price)}
                dy="-0.45em"
                textAnchor={r.left ? 'start' : 'end'}
                className="fill-current text-[10px]"
              >
                {r.label}
              </text>
            </g>
          ))}
          {series.map((s) => (
            <path
              key={s.key}
              d={path(s.botBuys)}
              fill="none"
              stroke={s.color}
              strokeWidth={2}
              strokeLinejoin="round"
              strokeLinecap="round"
            />
          ))}
          {marker !== null && (
            <g>
              <text
                x={x(marker.share)}
                y={PAD.top}
                textAnchor={
                  marker.share > 0.85
                    ? 'end'
                    : marker.share < 0.15
                      ? 'start'
                      : 'middle'
                }
                className="fill-current text-[10px] font-bold"
              >
                {marker.label}
              </text>
              {series.map((s) => (
                <circle
                  key={s.key}
                  cx={x(marker.share)}
                  cy={y(at(s.botBuys, marker.share))}
                  r={4}
                  fill={s.color}
                  stroke="var(--tx-bg-secondary)"
                  strokeWidth={2}
                />
              ))}
            </g>
          )}
          {hover !== null && (
            <line
              x1={x(hover)}
              x2={x(hover)}
              y1={PAD.top}
              y2={PAD.top + PLOT_H}
              stroke="var(--tx-border-secondary)"
            />
          )}
          <rect
            x={PAD.left}
            y={PAD.top}
            width={PLOT_W}
            height={PLOT_H}
            fill="transparent"
            onPointerMove={track}
            onPointerLeave={() => setHover(null)}
          />
        </svg>
        {hover !== null && (
          <div
            aria-hidden
            className="bg-surface ring-line-soft pointer-events-none absolute top-2 rounded-lg px-3 py-2 text-xs shadow-sm ring-1"
            style={
              hover < 0.55
                ? { left: `${((x(hover) + 12) / W) * 100}%` }
                : { right: `${((W - x(hover) + 12) / W) * 100}%` }
            }
          >
            <p className="mb-1 font-bold tabular-nums">
              {Math.round(hover * 100)}% {currency}
            </p>
            {series.map((s) => (
              <p
                key={s.key}
                className="text-muted flex items-center gap-1.5 tabular-nums"
              >
                <span
                  className="h-0.5 w-3 rounded-full"
                  style={{ background: s.color }}
                />
                {s.label}{' '}
                <span className="text-ink ml-auto pl-3">
                  {priceText(at(s.botBuys, hover))}
                </span>
              </p>
            ))}
          </div>
        )}
      </div>
      <p className="text-muted text-xs leading-relaxed">
        {currency} per {settlement}, venue fee included. Both prices rise as the{' '}
        {holder} fills with {currency}: the bot asks more {currency} for the{' '}
        {settlement} it pays and gives more for the {settlement} it takes, so it
        sheds {currency}.
      </p>
    </figure>
  )
}

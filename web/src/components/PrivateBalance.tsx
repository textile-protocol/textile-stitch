// A balance that hides behind dots when clicked.
//
// The masked state never renders the amount at all, not even transparent, so a
// screen reader, a copy-paste or devtools can't read it back. The dot count is
// fixed for the same reason: a mask as wide as the number gives its magnitude
// away.

import type { ReactNode } from 'react'

const DOTS = 5

type DotSize = 'xs' | 'sm' | 'md' | 'lg'

/** Dot and line box per text size, so masking a line never changes its height. */
const DOT_STYLES: Record<DotSize, { line: string; dot: string; gap: string }> = {
  xs: { line: 'h-4', dot: 'size-1.5', gap: 'gap-0.5' }, // text-xs
  sm: { line: 'h-5', dot: 'size-1.5', gap: 'gap-1' }, // text-sm
  md: { line: 'h-6', dot: 'size-2', gap: 'gap-1' }, // text-base
  lg: { line: 'h-9 sm:h-10', dot: 'size-2.5 sm:size-3', gap: 'gap-1.5' }, // text-3xl / sm:text-4xl
}

/** Five dots on one line of text of the given size. */
export function Dots({ size, className = '' }: { size: DotSize; className?: string }) {
  const s = DOT_STYLES[size]
  return (
    <span className={`flex items-center ${s.line} ${s.gap} ${className}`} aria-hidden>
      {Array.from({ length: DOTS }, (_, i) => (
        <span
          key={i}
          className={`balance-dot rounded-full bg-current ${s.dot}`}
          style={{ '--i': i } as React.CSSProperties}
        />
      ))}
    </span>
  )
}

export default function PrivateBalance({
  hidden,
  onToggle,
  children,
  masked,
  title,
  icon = 'md',
  align = 'end',
  className = '',
}: {
  hidden: boolean
  /** Flips every balance in the panel, not just this one. */
  onToggle: () => void
  /** The amount, shown when not hidden. */
  children: ReactNode
  /** What stands in for it: dots sized to match each line it replaces. */
  masked: ReactNode
  /** What the amount is, shown on hover while it's visible. */
  title?: string
  /** Size of the eye that appears on hover. */
  icon?: 'sm' | 'md' | 'lg'
  /** Which edge the amount hugs. The eye sits on the other side, so showing
   * it on hover never pushes the amount off its column. */
  align?: 'start' | 'end'
  className?: string
}) {
  const items = align === 'end' ? 'items-end' : 'items-start'
  const eye = (
    <EyeIcon
      closed={!hidden}
      size={icon}
      className="text-muted opacity-0 transition-opacity group-hover:opacity-100 group-focus-visible:opacity-100"
    />
  )
  return (
    <button
      type="button"
      onClick={onToggle}
      title={hidden ? 'Show balances' : (title ?? 'Hide balances')}
      className={`group -mx-2 -my-1 inline-flex cursor-pointer items-center gap-2 rounded-lg px-2 py-1 text-ink transition hover:bg-active active:scale-95 ${className}`}
    >
      {align === 'end' && eye}
      {hidden ? (
        <span key="dots" className={`flex flex-col ${items}`}>
          {masked}
        </span>
      ) : (
        <span key="amount" className={`balance-reveal flex flex-col ${items}`}>
          {children}
        </span>
      )}
      {align === 'start' && eye}
      <span className="sr-only">{hidden ? 'Balance hidden. Show balances' : 'Hide balances'}</span>
    </button>
  )
}

/** Pixel size and drawn stroke width per eye size. Whole-pixel boxes on the
 * 24-unit grid, and a stroke given in screen pixels rather than grid units, so
 * the outline lands on the same weight at every size instead of a blurry
 * fractional one. */
const EYE_SIZES: Record<'sm' | 'md' | 'lg', { px: number; stroke: number }> = {
  sm: { px: 16, stroke: 1.5 },
  md: { px: 18, stroke: 1.5 },
  lg: { px: 24, stroke: 2 },
}

/**
 * An eye, slashed when `closed`: the action the click takes, not the current
 * state, so it reads as "hide" over a visible amount.
 *
 * Paths are Lucide's `eye` and `eye-off` (ISC). The slashed one leaves gaps
 * where the slash crosses the outline, which is what keeps it legible at 16px;
 * a slash drawn over a whole eye smears into it.
 */
function EyeIcon({
  closed,
  size,
  className = '',
}: {
  closed: boolean
  size: 'sm' | 'md' | 'lg'
  className?: string
}) {
  const { px, stroke } = EYE_SIZES[size]
  return (
    <svg
      width={px}
      height={px}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      // Grid units that draw `stroke` screen pixels at this size.
      strokeWidth={(stroke * 24) / px}
      strokeLinecap="round"
      strokeLinejoin="round"
      shapeRendering="geometricPrecision"
      aria-hidden
      className={`block shrink-0 ${className}`}
    >
      {closed ? (
        <>
          <path d="M10.733 5.076a10.744 10.744 0 0 1 11.205 6.575 1 1 0 0 1 0 .696 10.747 10.747 0 0 1-1.444 2.49" />
          <path d="M14.084 14.158a3 3 0 0 1-4.242-4.242" />
          <path d="M17.479 17.499a10.75 10.75 0 0 1-15.417-5.151 1 1 0 0 1 0-.696 10.75 10.75 0 0 1 4.446-5.143" />
          <path d="m2 2 20 20" />
        </>
      ) : (
        <>
          <path d="M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0" />
          <circle cx="12" cy="12" r="3" />
        </>
      )}
    </svg>
  )
}

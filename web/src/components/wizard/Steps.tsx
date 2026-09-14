// The wizard's step rail, and the three label sets it can show.
//
// Sources sits before Spread in every set: the feed is where the price comes
// from, the spread is what the bot does with it, and an operator reasons in
// that order. (The short set already did; the long sets now match.)
//
// The rail is the only thing on screen that tells an operator how much road is
// left, so the three sets are deliberately different lengths: five pills for
// adding a corridor to a bot that already exists, seven for setting up a
// separate bot from the same start. Live is the last label in every set, even
// while the Connect screen is up: the email, the confirmation and the wait are
// states on the way there, not destinations of their own.

/** No bot on the picked chain, so the Where step never happens. Today's flow. */
export const LABELS = ['Corridor', 'Sources', 'Spread', 'Wallet', 'Approve', 'Live']

/** The operator saw the Where step and chose a separate bot. */
export const WHERE_LABELS = [
  'Corridor',
  'Where',
  'Sources',
  'Spread',
  'Wallet',
  'Approve',
  'Live',
]

/**
 * Adding to a bot that already exists. Wallet and the Connect ending are gone:
 * it has a wallet and a maker identity, and its confirmed address already
 * covers every corridor on that maker. Sources is Price feed alone, because the
 * RPC belongs to the bot and cannot differ per corridor. The writes run under
 * the Live pill rather than earning one of their own.
 */
export const SHORT_LABELS = ['Corridor', 'Where', 'Price feed', 'Spread', 'Live']

export default function Steps({
  current,
  labels,
}: {
  current: number
  labels: string[]
}) {
  return (
    <ol className="flex flex-wrap items-center gap-x-2 gap-y-1 text-sm">
      {labels.map((label, i) => (
        <li key={label} className="flex items-center gap-2">
          <span
            className={`flex size-6 items-center justify-center rounded-full text-xs font-bold ${
              i <= current ? 'bg-accent text-on-accent' : 'bg-hover text-faint'
            }`}
          >
            {i + 1}
          </span>
          <span className={i === current ? 'font-bold' : 'text-muted'}>{label}</span>
          {i < labels.length - 1 && <span className="text-faint">→</span>}
        </li>
      ))}
    </ol>
  )
}

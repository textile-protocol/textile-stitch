// The wizard's Skip link. Deliberately quiet: faint text, no button chrome, so
// the step's own controls stay the obvious way forward.
//
// Only on steps whose work the bot page can finish later (skip.ts decides
// where it lands): Approve (Tools has Approve allowances, the header has Start)
// and Live (the Corridors tab has the Textile card). Steps that need an answer
// before a bot can exist at all (corridor, where, sources, spread, wallet) have
// no link.

export default function SkipLink({
  onClick,
  title,
}: {
  onClick: () => void
  /** Where the skipped work lives now, shown on hover. */
  title: string
}) {
  return (
    <button
      type="button"
      title={title}
      onClick={onClick}
      className="text-xs text-faint underline-offset-2 hover:text-muted hover:underline"
    >
      Skip
    </button>
  )
}

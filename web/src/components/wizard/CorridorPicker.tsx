import { useMemo, useState } from 'react'
import type { Corridor } from '../../types'
import { Empty } from '../ui'
import {
  buildCorridorIndex,
  hasTwin,
  listingTag,
  seedDraft,
  tokenRowState,
  togglePick,
  type Draft,
  type RowState,
} from './corridorMatch'

/*
 * The wizard's corridor picker: a network rail on the left, the tokens listed
 * on that network on the right. The operator picks two tokens in either
 * order; after the first pick only tokens that share a deployed corridor with
 * it stay open, so the second pick always lands on a real corridor. The
 * picker owns the draft (network + up to two tokens) and reports the resolved
 * corridor id (or '') through onChange, so corridorId stays the wizard's
 * single source of truth. The Custom row at the bottom hands off to the
 * wizard's own form through onCustom.
 */

/** Brand-ish dot per chain. Inline style on purpose: Tailwind v4 can't see a
 * class built from a string. Unknown chains get the faint text colour. */
const CHAIN_DOT: Record<number, string> = {
  1: '#627eea',
  56: '#f0b90b',
  97: '#f0b90b',
  8453: '#0052ff',
  84532: '#0052ff',
  42220: '#fcff52',
  4663: '#00c805',
}
const dotColor = (chainId: number) => CHAIN_DOT[chainId] ?? 'var(--tx-text-tertiary)'

/* A blocked row is marked by a recessed background and a dimmed symbol, never
 * by opacity on the button. The note on the right ("no pair with cNGN") is the
 * only thing that says WHY the row can't be clicked, and a button-level
 * `opacity-50` multiplied with the note's own alpha: 0.45 x 0.5 left it at
 * about 1.6:1 against the card, unreadable on a phone. The note now carries
 * full `text-muted` (about 4.7:1 light, 6.8:1 dark). The symbol stays dim on
 * purpose; WCAG 1.4.3 exempts a disabled control's own label. */
const ROW_CLASS: Record<RowState, string> = {
  picked: 'cursor-pointer border-accent bg-accent-tint',
  open: 'cursor-pointer border-line-soft hover:bg-hover',
  pending: 'cursor-not-allowed border-line-soft bg-canvas',
  nopair: 'cursor-not-allowed border-line-soft bg-canvas',
  full: 'cursor-not-allowed border-line-soft bg-canvas',
}

const ROW_BASE = 'flex w-full items-center gap-3 rounded-lg border p-3 text-left'

/** The row note, at a weight a phone can actually read. */
function RowNote({ children }: { children: string }) {
  return <span className="ml-auto pl-2 text-right text-xs text-muted">{children}</span>
}

function Tick() {
  return (
    <span aria-hidden className="ml-auto text-sm font-bold text-accent">
      ✓
    </span>
  )
}

export default function CorridorPicker({
  corridors,
  value,
  onChange,
  customSelected,
  onCustom,
}: {
  corridors: Corridor[]
  /** The wizard's corridorId: a catalog id, '' or the custom sentinel. */
  value: string
  /** Called with the resolved corridor id, or '' while the pair is incomplete. */
  onChange: (corridorId: string) => void
  /** The wizard's corridorId is the custom sentinel: highlight the Custom row. */
  customSelected: boolean
  /** The Custom row was clicked. The wizard sets the sentinel and opens its form. */
  onCustom: () => void
}) {
  const index = useMemo(() => buildCorridorIndex(corridors), [corridors])
  // The draft is seeded once from corridorId, so a preselected corridor shows
  // as resolved and a return from the custom form starts clean. While mounted
  // only this component changes corridorId.
  const [draft, setDraft] = useState<Draft>(() => seedDraft(index, value))

  const net = index.networks.find((n) => n.chainId === draft.chainId)
  const resolved = index.byId.get(value)
  const first = draft.picks[0]

  const pickNetwork = (chainId: number) => {
    if (chainId === draft.chainId) return
    setDraft({ chainId, picks: [] })
    onChange('')
  }

  const clickToken = (symbol: string) => {
    const next = togglePick(index, draft, symbol)
    setDraft(next.draft)
    onChange(next.corridorId)
  }

  const pickExtra = (c: Corridor) => {
    setDraft({ chainId: draft.chainId, picks: [] })
    onChange(c.id)
  }

  const rowNote = (state: RowState): string | null => {
    if (state === 'pending') return 'not deployed yet'
    if (state === 'nopair') return `no pair with ${first ?? ''}`
    return null
  }

  // Two tokens resolve to the first deployed listing in catalog order. When
  // the pair has more than one, say which one that was and that the other is
  // in the rows below, rather than commit the bot to a feed silently.
  const picked = resolved
    ? `${resolved.displayName} picked${
        hasTwin(index, resolved) ? ` (listing ${listingTag(resolved)}, another is below)` : ''
      }`
    : ''

  // The one line that tells the operator where they are. No other explainer.
  const hint =
    !net || net.tokens.length === 0
      ? ''
      : resolved && draft.picks.length === 2
        ? `${picked}. Unpick one to change.`
        : resolved
          ? picked
          : first !== undefined
            ? `Pick what ${first} trades against`
            : 'Pick two'

  return (
    <div>
      {index.networks.length === 0 ? (
        <Empty title="No corridors listed">Set up a custom corridor below.</Empty>
      ) : (
        <div className="flex flex-col gap-4 sm:flex-row sm:gap-6">
          <aside className="shrink-0 border-b border-line-soft pb-4 sm:w-[276px] sm:border-b-0 sm:border-r sm:pb-0 sm:pr-6">
            <p className="mb-2 text-sm font-bold">Network</p>
            {/* Wrapping chips on a phone, a plain column on a wide screen. The
                column must NOT wrap: a wrapping column flex line is sized to
                its widest item, so the pills kept their max-content width,
                overflowed the rail and painted under the token rows. Nowrap
                stretches them to the rail instead, which is also what makes
                the label's truncate work if a chain name ever outgrows it. */}
            <div className="flex flex-wrap gap-1 sm:flex-col sm:flex-nowrap">
              {index.networks.map((n) => {
                const active = n.chainId === draft.chainId
                return (
                  <button
                    key={n.chainId}
                    type="button"
                    aria-pressed={active}
                    onClick={() => pickNetwork(n.chainId)}
                    className={`flex items-center gap-2 rounded-full border px-3 py-1.5 text-left text-base font-bold transition ${
                      active
                        ? 'border-accent bg-accent-tint'
                        : 'border-transparent hover:bg-hover'
                    }`}
                  >
                    <span
                      aria-hidden
                      className="size-2.5 shrink-0 rounded-full border border-line-soft"
                      style={{ background: dotColor(n.chainId) }}
                    />
                    <span className="min-w-0 flex-1 truncate">{n.label}</span>
                  </button>
                )
              })}
            </div>
          </aside>

          {net && (
            <div className="min-w-0 flex-1">
              <p className="mb-2 text-sm">
                <span className="font-bold">Tokens on {net.label}</span>
                {hint && <span className="text-muted"> - {hint}</span>}
              </p>
              <div className="space-y-2">
                {net.tokens.map((t) => {
                  const state = tokenRowState(index, draft, t)
                  const note = rowNote(state)
                  const blocked = state !== 'picked' && state !== 'open'
                  return (
                    <button
                      key={t.key}
                      type="button"
                      aria-pressed={state === 'picked'}
                      disabled={blocked}
                      onClick={() => clickToken(t.symbol)}
                      className={`${ROW_BASE} ${ROW_CLASS[state]}`}
                    >
                      <span className={`font-bold ${blocked ? 'text-faint' : ''}`}>
                        {t.symbol}
                      </span>
                      {state === 'picked' && <Tick />}
                      {note && <RowNote>{note}</RowNote>}
                    </button>
                  )
                })}
              </div>

              {net.extras.length > 0 && (
                <>
                  <p className="mb-2 mt-4 text-sm">
                    <span className="font-bold">Also on {net.label}</span>
                    <span className="text-muted"> - pick as listed</span>
                  </p>
                  <div className="space-y-2">
                    {net.extras.map((c) => {
                      const active = c.id === value
                      // A second listing of a pair the grid above already
                      // resolves. Both rows would otherwise read the same, so
                      // this one carries the id that tells them apart.
                      const twin = hasTwin(index, c)
                      return (
                        <button
                          key={c.id}
                          type="button"
                          aria-pressed={active}
                          disabled={c.pendingDeploy}
                          onClick={() => pickExtra(c)}
                          className={`${ROW_BASE} ${
                            c.pendingDeploy
                              ? ROW_CLASS.pending
                              : active
                                ? ROW_CLASS.picked
                                : ROW_CLASS.open
                          }`}
                        >
                          <span
                            className={`font-bold ${c.pendingDeploy ? 'text-faint' : ''}`}
                          >
                            {c.displayName}
                          </span>
                          {twin && (
                            <span className="min-w-0 truncate font-mono text-xs text-muted">
                              {listingTag(c)}
                            </span>
                          )}
                          {active && <Tick />}
                          {c.pendingDeploy && <RowNote>not deployed yet</RowNote>}
                        </button>
                      )
                    })}
                  </div>
                </>
              )}
            </div>
          )}
        </div>
      )}

      {/* Custom: a pair the catalog doesn't ship. One click opens the details
          form. The row stays highlighted after Back because corridorId is
          still the sentinel, and the wizard's Next reopens the form. */}
      <button
        type="button"
        aria-pressed={customSelected}
        onClick={onCustom}
        className={`mt-4 ${ROW_BASE} ${
          customSelected
            ? 'border-accent bg-accent-tint'
            : 'border-line-soft hover:bg-hover'
        }`}
      >
        <span className="font-bold">Custom corridor</span>
        <span className="text-sm text-muted">Your own tokens, RPC and price feed</span>
      </button>
    </div>
  )
}

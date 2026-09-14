// The Where step: the corridor is picked, and the operator already runs a bot
// on its chain. One row per same-chain bot, then a separate-bot row last.
//
// It never asks "a new bot or a new corridor". It states the two outcomes:
// adding to a bot that exists uses the wallet and the money already in it, or a
// separate bot brings its own of both. Adding is recommended and first. The two
// real consequences of sharing are on the screen, not in a tooltip: the
// corridors share that wallet, and adding one restarts the bot.
//
// Bots that can't take the corridor are shown with the reason, not hidden. An
// operator who came here to add it to one particular bot has to be told why
// that bot isn't on offer.

import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { api } from '../../api'
import { botPath } from '../../botRoutes'
import { shortAddress } from '../../format'
import { Banner, Button, Card, StatePill, Tag } from '../ui'
import { balanceLine, type Candidate } from './candidates'
import { place } from './wizardCopy'
import type { Corridor } from '../../types'

export interface WhereStepProps {
  corridor: Corridor
  candidates: Candidate[]
  /** The row the radio sits on. Null is the separate-bot row. */
  selected: string | null
  /** Every change of the radio, not only the press of Next. */
  onSelect: (bot: string | null) => void
  /** Why a prefilled target was dropped. Shown above the rows. */
  notice?: string | null
  /** Null means the separate-bot row. */
  onChoose: (bot: string | null) => void
  onBack: () => void
}

/** A funding read per eligible row is real load on a slow RPC. Cap the fan-out. */
const MAX_BALANCE_READS = 4

/**
 * The row this screen opens on. Null is the separate-bot row.
 *
 * The preferred bot wins when it can take the corridor. When it CAN'T, the
 * answer is the separate-bot row and never a different bot: `?bot=` names the
 * wallet the operator meant, and moving the corridor onto someone else's money
 * is a decision, not a default. Falling through to the first eligible row made
 * the next press of Next, which is the muscle-memory action on every other step
 * of this wizard, spend a bot they never named.
 *
 * With nothing preferred, the first eligible row: nobody named anything, so
 * preselecting the recommended option costs nobody a wallet they didn't choose.
 *
 * Lives out here, and the selection lives in the page, so the rail above this
 * screen can read the same choice and show the short road while a bot row is
 * selected.
 */
export function defaultChoice(
  candidates: Candidate[],
  preferred: string | null,
): string | null {
  const eligible = candidates.filter((c) => c.eligible)
  if (preferred)
    return eligible.some((c) => c.bot.name === preferred) ? preferred : null
  return eligible[0]?.bot.name ?? null
}

export default function WhereStep({
  corridor,
  candidates,
  selected,
  onSelect,
  notice = null,
  onChoose,
  onBack,
}: WhereStepProps) {
  const eligible = candidates.filter((c) => c.eligible)
  // The rest are not choices, so they are folded away under the ones that are.
  // They are still on the screen, because "why isn't my bot here" is a question
  // this screen has to answer for every bot, not only for a prefilled one: an
  // operator whose own bot already quotes the corridor was otherwise told the
  // corridor needed a bot of its own.
  const blocked = candidates.filter((c) => !c.eligible)
  // The one blocked reason that changes what the screen says rather than only
  // which row is greyed out.
  const already = blocked.filter((c) => c.blockedKind === 'already')

  const [balances, setBalances] = useState<Record<string, string[]>>({})

  // Balances, in the background, for the eligible rows only. They are what makes
  // "uses the money already in it" checkable, but nothing waits on them: a slow
  // or failed read leaves the row showing its wallet and corridors alone. No
  // spinner and no error in their place.
  useEffect(() => {
    let live = true
    const names = eligible.slice(0, MAX_BALANCE_READS).map((c) => c.bot.name)
    for (const name of names) {
      void api
        .funding(name)
        .then((funding) => {
          if (!live) return
          const line = balanceLine(funding)
          if (line.length > 0) setBalances((prev) => ({ ...prev, [name]: line }))
        })
        .catch(() => {})
    }
    return () => {
      live = false
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [candidates])

  return (
    <Card title={place.title(corridor.displayName)}>
      <div className="space-y-4">
        {notice && <Banner tone="info">{notice}</Banner>}

        <div className="space-y-2">
          {/* One line: what sharing a bot means for the money, which is the
              whole basis of the choice. No count of bots, no Recommended
              badge, no consequence bullets. The screen only ever appears when
              there is a real choice, so it can be a choice rather than a
              briefing. */}
          {eligible.length > 0 && (
            <p className="text-sm text-muted">{place.recommendedWhy}</p>
          )}

          {/* Nothing to add to. Which sentence depends on why: a bot that is
              already quoting the corridor is the one case where a separate bot
              is not the answer, so it is named and linked instead. */}
          {eligible.length === 0 &&
            (already.length > 0 ? (
              <div className="space-y-1">
                <p className="text-sm">
                  {place.allBlockedAlready(
                    already.map((c) => c.bot.name),
                    corridor.displayName,
                  )}
                </p>
                <p className="text-sm text-muted">
                  {place.alreadyNext(corridor.displayName)}
                </p>
                <div className="flex flex-wrap gap-x-4">
                  {already.map((c) => (
                    <Link
                      key={c.bot.name}
                      to={botPath(c.bot.name)}
                      className="text-sm underline"
                    >
                      {place.openBot(c.bot.name)}
                    </Link>
                  ))}
                </div>
              </div>
            ) : (
              <p className="text-sm">
                {candidates.every((c) => c.scanned)
                  ? place.allBlocked(corridor.displayName)
                  : place.allBlockedPartial(corridor.displayName)}
              </p>
            ))}

          {eligible.map((c) => (
            <Row
              key={c.bot.name}
              candidate={c}
              balances={balances[c.bot.name] ?? null}
              active={selected === c.bot.name}
              onSelect={() => onSelect(c.bot.name)}
            />
          ))}

        </div>

        <label
          className={rowClass(selected === null, false)}
          onClick={() => onSelect(null)}
        >
          <input
            type="radio"
            name="where"
            checked={selected === null}
            onChange={() => onSelect(null)}
            className="mt-1 accent-[var(--tx-accent)]"
          />
          <span className="min-w-0 flex-1">
            <span className="block font-bold">{place.separateTitle}</span>
            <span className="mt-0.5 block text-sm text-muted">
              {place.separateBody}
            </span>
          </span>
        </label>

        <div className="flex justify-between">
          <Button onClick={onBack}>{place.back}</Button>
          <Button variant="primary" onClick={() => onChoose(selected)}>
            {place.next}
          </Button>
        </div>
      </div>
    </Card>
  )
}

function rowClass(active: boolean, disabled: boolean): string {
  return `flex items-start gap-3 rounded-lg border p-3 ${
    disabled
      ? 'cursor-not-allowed border-line-soft opacity-50'
      : active
        ? 'cursor-pointer border-accent bg-accent-tint'
        : 'cursor-pointer border-line-soft hover:bg-hover'
  }`
}

function Row({
  candidate,
  balances,
  active,
  onSelect,
}: {
  candidate: Candidate
  balances: string[] | null
  active: boolean
  onSelect?: () => void
}) {
  const { bot } = candidate
  const disabled = !candidate.eligible
  return (
    <label
      className={rowClass(active, disabled)}
      onClick={disabled ? undefined : onSelect}
    >
      <input
        type="radio"
        name="where"
        checked={active}
        disabled={disabled}
        onChange={() => onSelect?.()}
        className="mt-1 accent-[var(--tx-accent)]"
      />
      <span className="min-w-0 flex-1">
        <span className="flex flex-wrap items-center gap-2">
          <span className="font-bold">{place.addTo(bot.name)}</span>
          <StatePill state={bot.state} status={bot.status} venue={bot.config?.venue} />
          {bot.config && <Tag>chain {bot.config.chainId}</Tag>}
        </span>
        {candidate.blocked ? (
          <span className="mt-0.5 block text-sm text-muted">{candidate.blocked}</span>
        ) : (
          <>
            <span className="mt-0.5 block text-sm text-muted">
              {place.quotes(candidate.pairs)}{' '}
              {candidate.operatorAddress &&
                place.wallet(shortAddress(candidate.operatorAddress))}
            </span>
            {balances && (
              <span className="mt-0.5 block text-sm text-muted">
                {place.holds(balances)}
              </span>
            )}
          </>
        )}
      </span>
    </label>
  )
}

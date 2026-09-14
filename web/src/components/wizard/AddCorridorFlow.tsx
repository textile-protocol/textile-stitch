// The last screen of the add-a-corridor path: everything that writes, then the
// ending. One click on the Spread step gets here and nothing else is asked.
//
// Order, and why each part is here:
//
//  1. addPool. Appends the corridor as another `[[pools]]` entry and restarts a
//     running bot, so its other corridors stop quoting for the bounce.
//  2. saveSettings, ONLY when the operator changed the spread or the feed. The
//     template already carries both, so taking the defaults is one restart
//     fewer. Never `rpcUrl`: that belongs to the bot, not to one corridor.
//  3. enrollRfq. Not optional and not a nicety. `rfq_corridor` is per pool and a
//     pool from addPool has none, while `[rfq].enabled` is bot-level and already
//     true, so the start runner's seat stage short-circuits and would happily
//     start a bot that answers nothing on the new corridor. Enrollment re-stamps
//     every pool and is idempotent. Nothing is asked of Textile: a confirmed
//     email seats a maker, once, across every corridor and chain.
//  4. The funding check, on the new corridor's own two tokens. Then the shared
//     start runner: approve spending, check the seats, start, confirm it stayed
//     up.
//
// It drives `useStartSequence` itself rather than mounting the Fund step. The
// Fund step reports "live" the moment it sees a running bot, which is right for
// a bot it just created and wrong here: the bot is already running, and the new
// corridor's token still needs its Permit2 approval.

import { useCallback, useEffect, useRef, useState } from 'react'
import { api } from '../../api'
import { formatClock } from '../../format'
import { pairSymbols } from '../SpreadExample'
import { Banner, Button, Card, Spinner } from '../ui'
import EmailVerifyWait from './EmailVerifyWait'
import { AddressBlock, GasRow, TokenRow } from './FundingRows'
import ProgressList, { type ProgressRow } from './ProgressList'
import { pairFunded, templatePair, templateSpreads } from './candidates'
import { type FundOutcome } from './FundStep'
import { clearAddResume, saveAddResume } from './resume'
import { errorText, useStartSequence } from './useStartSequence'
import { add, botRunState, fund, progress as progressCopy } from './wizardCopy'
import type { Corridor, Funding, FundingToken, SaveResult, Spread } from '../../types'

export interface AddCorridorFlowProps {
  bot: string
  corridor: Corridor
  /**
   * Whether the target bot was quoting when the operator chose it. A stopped
   * bot is not restarted by the add, it is started, and every corridor already
   * on it goes back on the book. Different sentence, not a smaller one.
   */
  botRunning: boolean
  /**
   * Whether there was a live process at all: running, restarting or paused.
   * The panel's own word for it, so the list of Docker states stays server-side.
   *
   * It is what separates the two ways `restarted` comes back false. A paused or
   * restarting bot holds its old config in memory and keeps it, which is worth
   * a warning; a stopped one had nothing to restart and is started a few rows
   * below, which is not.
   */
  botCanStop: boolean
  spreads: { buy: Spread; sell: Spread }
  /** The operator's own feed URL, or null to keep the corridor's own. */
  ownFeedUrl: string | null
  /** Already written by an earlier run: a reload after the add landed. */
  addedPoolIndex: number | null
  /** Fired once the add lands, so the wizard can put it in the URL. */
  onAdded: (poolIndex: number) => void
  /** Back to the Spread step. Offered only before the writes start. */
  onBack: () => void
  /**
   * Leave this lane and start the wizard again, clearing BOTH resume records.
   *
   * The way out of an enrolment that keeps failing. The corridor is on disk by
   * then, and the record that covers that window reopens this lane on every
   * visit to /add until it is cleared, so without this the panel's own Add
   * corridor button leads nowhere else for a day.
   */
  onStartOver: () => void
  onOpenBot: () => void
}

type WriteKey = 'add' | 'save' | 'enroll'
type WriteState = 'pending' | 'running' | 'done' | 'failed' | 'skipped'
type Phase = 'writing' | 'funding' | 'running' | 'done'

const POLL_MS = 5000

export default function AddCorridorFlow({
  bot,
  corridor,
  botRunning,
  botCanStop,
  spreads,
  ownFeedUrl,
  addedPoolIndex,
  onAdded,
  onBack,
  onStartOver,
  onOpenBot,
}: AddCorridorFlowProps) {
  const [writes, setWrites] = useState<Record<WriteKey, WriteState>>({
    add: 'pending',
    save: 'pending',
    enroll: 'pending',
  })
  const [phase, setPhase] = useState<Phase>('writing')
  const [failedStep, setFailedStep] = useState<WriteKey | null>(null)
  const [failedMessage, setFailedMessage] = useState<string | null>(null)
  // The bot kept its old config: a restart that failed, or a live process that
  // could not be bounced. `restartError` is the server's own words for the
  // first; the second has none, and the server's prose message is not used for
  // either, because its tail tells the operator to approve tokens and enroll
  // the maker by hand, which is what the two rows below are doing for them.
  const [notBounced, setNotBounced] = useState(false)
  const [restartError, setRestartError] = useState<string | null>(null)
  const [poolIndex, setPoolIndex] = useState<number | null>(addedPoolIndex)
  const [funding, setFunding] = useState<Funding | null>(null)
  const [fundingError, setFundingError] = useState<string | null>(null)
  const [checkedAt, setCheckedAt] = useState<number | null>(null)
  const [outcome, setOutcome] = useState<FundOutcome | null>(null)
  const [checking, setChecking] = useState(false)
  const [stopping, setStopping] = useState(false)
  const [stopError, setStopError] = useState<string | null>(null)
  const [attempt, setAttempt] = useState(0)

  const mountedRef = useRef(true)
  const startedRef = useRef(-1)
  const onAddedRef = useRef(onAdded)
  onAddedRef.current = onAdded

  // The two booleans as one state, so the row's sub-line and the "still running
  // its old config" banner below it split on the same three cases. Told only
  // `running`, the row promised to START a paused bot on the same screen as a
  // banner saying it was still up on its old config.
  const runState = botRunState(botRunning, botCanStop)

  const pair = templatePair(corridor.tomlTemplate)
  const template = templateSpreads(corridor.tomlTemplate)
  const spreadChanged =
    spreads.buy.kind !== 'bps' ||
    spreads.sell.kind !== 'bps' ||
    spreads.buy.value.trim() !== (template.buy ?? '1') ||
    spreads.sell.value.trim() !== (template.sell ?? '1')
  // A resumed run has the corridor already written with the template's own
  // spreads and feed, and the earlier steps opened on those defaults again, so
  // there is nothing of the operator's to save.
  //
  // Read ONCE, at mount, and never from the live prop: that prop goes non-null
  // the moment THIS run's own add lands. Read later it says "an earlier run
  // wrote this" about work from thirty seconds ago, which is how a Retry next
  // to "Your changes did not save" came to re-run enrolment alone, leave the
  // corridor on Textile's own spreads and feed, and take the row off the list
  // rather than say so.
  const resumedFromDisk = useRef(addedPoolIndex !== null).current
  const needsPatch = !resumedFromDisk && (spreadChanged || ownFeedUrl !== null)
  // Whether the patch actually landed, which is a different question from
  // whether a pool exists. Only this closes the save.
  const patchDoneRef = useRef(false)
  // The written pool's own pair, kept for a retry. The save sends the index and
  // the pair together, because an index only means anything against the list
  // that was read, and on a retry the add result that carried them is gone.
  const patchPairRef = useRef<{ collateral: string; debt: string } | null>(null)

  const softRow = pair
    ? (funding?.tokens.find((t) => t.token.toLowerCase() === pair.collateral) ?? null)
    : null
  const stableRow = pair
    ? (funding?.tokens.find((t) => t.token.toLowerCase() === pair.debt) ?? null)
    : null
  // This corridor is quoted against something the panel has no dollar price
  // for, so neither of its rows can ever be valued and the funding check
  // refuses whatever arrives. Read off the side the pool is quoted in, which is
  // the one that decides: `gate.unpriceable` only speaks for the whole bot, and
  // a sibling corridor holding USDT keeps it false. Not "add money": no amount
  // of either token changes this answer, so the lane says so and stops polling
  // instead of asking for some every five seconds forever.
  const pairUnpriceable =
    funding !== null &&
    (stableRow?.unpriceable === true || (pair === null && funding.gate.unpriceable))

  const runner = useStartSequence(bot, {
    onFunding: (next) => {
      if (mountedRef.current) setFunding(next)
    },
    onOutcome: (o) => {
      // Enrolment already cleared this on the way through. Repeated here so no
      // ending can leave one standing, whatever route reached it.
      clearAddResume()
      if (!mountedRef.current) return
      setOutcome(o)
      setPhase('done')
    },
  })
  const { run: startRun } = runner

  useEffect(() => {
    mountedRef.current = true
    return () => {
      mountedRef.current = false
    }
  }, [])

  const readFunding = useCallback(async () => {
    try {
      const next = await api.funding(bot)
      if (!mountedRef.current) return
      setFunding(next)
      setFundingError(null)
      setCheckedAt(Date.now())
      if (pairFunded(next, pair)) {
        setPhase('running')
        startRun()
      } else {
        setPhase('funding')
      }
    } catch (e) {
      if (!mountedRef.current) return
      setFundingError(errorText(e))
      setPhase('funding')
    }
    // `pair` is derived from the corridor prop, which does not change here.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [bot, startRun])

  // The writes, once per attempt. Retry bumps `attempt`; a step already done
  // keeps its tick and is not sent again.
  useEffect(() => {
    if (startedRef.current === attempt) return
    startedRef.current = attempt

    const mark = (key: WriteKey, state: WriteState) => {
      if (mountedRef.current) setWrites((prev) => ({ ...prev, [key]: state }))
    }
    // Nothing this attempt had to do. A row that already ticked keeps its tick:
    // a retry re-reporting finished work as skipped is a retry misreporting
    // what the operator watched happen.
    const skip = (key: WriteKey) => {
      if (mountedRef.current)
        setWrites((prev) => ({
          ...prev,
          [key]: prev[key] === 'done' ? 'done' : 'skipped',
        }))
    }
    const fail = (key: WriteKey, message: string) => {
      if (!mountedRef.current) return
      mark(key, 'failed')
      setFailedStep(key)
      setFailedMessage(message)
    }

    void (async () => {
      let index = poolIndex

      if (index === null) {
        mark('add', 'running')
        let added: SaveResult
        try {
          added = await api.addPool(bot, corridor.id)
        } catch (e) {
          fail('add', errorText(e))
          return
        }
        if (!mountedRef.current) return
        index = added.settings.poolIndex
        setPoolIndex(index)
        // Kept for a retry, which runs long after this result is gone.
        const pool = added.settings.pools.find((p) => p.index === index)
        patchPairRef.current = {
          collateral: pool?.collateral ?? added.settings.pair.collateral,
          debt: pool?.debt ?? added.settings.pair.debt,
        }
        // Written to disk and not yet enrolled: from here on a closed tab has
        // to be recoverable, or the bot quotes nothing on the new corridor and
        // looks healthy doing it.
        saveAddResume(bot, corridor.id, index)
        onAddedRef.current(index)
        // `restarted` comes back false two ways. A live process that could not
        // be bounced (a failed restart, or a paused one holding its old config
        // in memory) is trouble and says so. A bot that was simply stopped is
        // not: there was nothing to restart, and the wizard starts it a few rows
        // below. Warning on that one told the operator to go and do by hand what
        // this screen was doing for them.
        if (!added.restarted && (added.restartError || botCanStop)) {
          setNotBounced(true)
          setRestartError(added.restartError)
        }
        mark('add', 'done')
      } else {
        skip('add')
      }

      // Driven off the pool index and the patch's own flag, not off whether
      // THIS attempt was the one that wrote the pool. A retry after a failed
      // save has the pool from the attempt before it, and the operator's spread
      // and feed still unsaved: that is precisely the run that has to send it.
      const patchPair = patchPairRef.current
      if (index !== null && needsPatch && !patchDoneRef.current && patchPair) {
        mark('save', 'running')
        try {
          await api.saveSettings(bot, {
            // The pair goes with the index: the panel refuses a multi-corridor
            // write whose index someone else's add or remove has renumbered.
            pool: index,
            collateral: patchPair.collateral,
            debt: patchPair.debt,
            ...(spreadChanged ? { buy: spreads.buy, sell: spreads.sell } : {}),
            ...(ownFeedUrl !== null ? { feedUrl: ownFeedUrl } : {}),
          })
        } catch (e) {
          fail('save', errorText(e))
          return
        }
        if (!mountedRef.current) return
        patchDoneRef.current = true
        mark('save', 'done')
      } else {
        skip('save')
      }

      mark('enroll', 'running')
      try {
        await api.enrollRfq(bot)
      } catch (e) {
        fail('enroll', errorText(e))
        return
      }
      if (!mountedRef.current) return
      mark('enroll', 'done')
      // The window the record covers closes here: the pool is on disk AND
      // Textile knows about it, so nothing is left half-done if this tab goes
      // away. Cleared now rather than at the ending, because the funding wait
      // can last as long as it takes money to arrive and a record left standing
      // that long would reopen this lane on every later visit to /add.
      clearAddResume()

      await readFunding()
    })()
    // Deliberately keyed on the attempt alone: everything else is read at the
    // moment the attempt starts.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [attempt])

  // Watch the wallet until the new corridor has a side it can quote. Stops on a
  // corridor the panel can't value: that answer is the same every time.
  useEffect(() => {
    if (phase !== 'funding' || pairUnpriceable) return
    const id = window.setInterval(() => void readFunding(), POLL_MS)
    return () => window.clearInterval(id)
  }, [phase, pairUnpriceable, readFunding])

  function retry() {
    setFailedStep(null)
    setFailedMessage(null)
    setWrites((prev) => ({
      add: prev.add === 'failed' ? 'pending' : prev.add,
      save: prev.save === 'failed' || prev.add === 'failed' ? 'pending' : prev.save,
      enroll: 'pending',
    }))
    setAttempt((n) => n + 1)
  }

  async function stopBlocking(name: string) {
    setStopping(true)
    setStopError(null)
    try {
      await api.stop(name)
      if (mountedRef.current) runner.retry()
    } catch (e) {
      if (mountedRef.current) setStopError(errorText(e))
    } finally {
      if (mountedRef.current) setStopping(false)
    }
  }

  const symbols = pairSymbols(corridor.displayName)
  const waitingForMoney = phase === 'funding' && !pairUnpriceable
  // Live on the buy side only: the wallet has the stable but not the soft
  // token, so the bot can buy and has nothing to sell. Worth one sentence.
  const oneSided = softRow?.funded !== true && stableRow?.funded === true

  // The ending: the bot's own page, or the confirm-your-email screen.
  if (phase === 'done' && outcome) {
    return (
      <div className="space-y-4">
        {oneSided && symbols && (
          <Banner tone="info">
            {add.oneSided(bot, symbols.base, symbols.quote)}
          </Banner>
        )}
        {/* Not once the bot is up: the runner started it, and a banner telling
            the operator to go and start it by hand would contradict the screen
            it is sitting on. */}
        {notBounced && outcome.kind !== 'live' && (
          <Banner tone="warning">
            {restartError && <p>{restartError}</p>}
            <p className={restartError ? 'mt-1' : undefined}>
              {add.notRestarted(bot)}
            </p>
          </Banner>
        )}
        {outcome.kind === 'live' ? (
          // Live: the bot page is the destination, same as the new-bot lane.
          // The banners above are the only thing worth a click first.
          <div>
            <Button variant="primary" onClick={onOpenBot}>
              {add.openBot}
            </Button>
          </div>
        ) : (
          // No Back: this bot already has a maker identity, and the address
          // that seats it is confirmed once per maker, not per corridor.
          <EmailVerifyWait
            bot={bot}
            initial={outcome.status}
            initialError={outcome.kind === 'waiting' ? outcome.error : null}
            onApproved={onOpenBot}
          />
        )}
      </div>
    )
  }

  const seq = runner.state
  const failure = seq.failure
  const gasSymbol = funding?.gas.symbol ?? 'gas'
  const address = funding?.operatorAddress ?? null

  const rows: ProgressRow[] = [
    {
      key: 'add',
      title: add.addRow(corridor.displayName, bot),
      sub: writes.add === 'skipped' ? undefined : add.addRowSub(runState),
      state: writes.add,
    },
  ]
  if (needsPatch) {
    rows.push({
      key: 'save',
      title: add.saveRow,
      sub: add.saveRowSub,
      state: writes.save,
    })
  }
  rows.push({
    key: 'enroll',
    title: add.enrollRow(bot, corridor.displayName),
    sub: add.enrollRowSub,
    state: writes.enroll,
  })
  if (phase !== 'writing') {
    rows.push(
      {
        key: 'approve',
        title: progressCopy.approveTitle(seq.approveTokens, seq.progress.approve),
        sub: seq.progress.approve === 'skipped' ? undefined : progressCopy.approveSub,
        state: seq.progress.approve,
        children:
          seq.stage === 'approve-busy' ? (
            <p className="mt-1 text-xs text-muted">{progressCopy.walletBusy}</p>
          ) : undefined,
      },
      { key: 'access', title: progressCopy.accessTitle, state: seq.progress.access },
      {
        key: 'start',
        title: progressCopy.startTitle,
        state: seq.progress.start,
        children:
          seq.stage === 'start-busy' ? (
            <p className="mt-1 text-xs text-muted">{progressCopy.startBusy}</p>
          ) : undefined,
      },
      {
        key: 'verify',
        title: progressCopy.verifyTitle,
        sub: progressCopy.verifySub,
        state: seq.progress.verify,
      },
    )
  }

  const needs: string[] = []
  if (funding && waitingForMoney) {
    const symbolsHere = [softRow?.symbol, stableRow?.symbol].filter(
      (s): s is string => !!s,
    )
    if (softRow?.funded !== true && stableRow?.funded !== true && symbolsHere.length > 0) {
      needs.push(fund.needsSide(funding.gate.minTokenUsd, symbolsHere))
    }
    if (funding.gas.ok === false) {
      needs.push(fund.needsGas(funding.gate.minGasUsd, funding.gas.symbol))
    }
  }

  return (
    <Card title={add.workTitle(corridor.displayName, bot)}>
      <div className="space-y-4">
        <ProgressList rows={rows} />

        {notBounced && (
          <Banner tone="warning">
            {restartError && <p>{restartError}</p>}
            <p className={restartError ? 'mt-1' : undefined}>
              {add.notRestarted(bot)}
            </p>
          </Banner>
        )}

        {/* Every refusal comes back in the server's own words: it knows whether
            the bot needs updating, whether the feed window is too narrow, or
            whether something else already quotes this corridor. */}
        {failedStep && failedMessage && (
          <Banner tone="danger">
            <div className="space-y-2">
              <p>{failedMessage}</p>
              {failedStep === 'add' && <p>{add.addFailed(bot)}</p>}
              {failedStep === 'save' && <p>{add.saveFailed(corridor.displayName)}</p>}
              {/* Enrolment is the one failure with the corridor already on
                  disk, so Back would be a lie and Retry alone was a dead end:
                  a venue that keeps refusing left the operator on this screen
                  with one button, and the record behind it reopened the same
                  screen on every later visit. Say where the corridor stands,
                  and offer both ways off it. */}
              {failedStep === 'enroll' && (
                <>
                  <p>{add.enrollFailed(failedMessage, bot)}</p>
                  <p>{add.enrollLeave(bot)}</p>
                </>
              )}
              <div className="flex flex-wrap gap-2">
                {failedStep === 'add' && (
                  <Button onClick={onBack}>{add.back}</Button>
                )}
                <Button variant="primary" onClick={retry}>
                  {add.retry}
                </Button>
                {failedStep === 'enroll' && (
                  <>
                    <Button onClick={onOpenBot}>{add.openBot}</Button>
                    <Button onClick={onStartOver}>{add.enrollStartOver}</Button>
                  </>
                )}
              </div>
            </div>
          </Banner>
        )}

        {/* The corridor is written and Textile has it, and the panel cannot
            price either of its sides, so the check it would wait on can never
            pass. Terminal, like the Fund step's own version of this screen: an
            address to send to would be asking for money that cannot help. */}
        {phase === 'funding' && pairUnpriceable && funding && (
          <Banner tone="warning">
            <div className="space-y-2">
              <p className="font-bold">{add.unpriceableTitle}</p>
              <p>
                {add.unpriceable(
                  corridor.displayName,
                  bot,
                  [softRow?.symbol, stableRow?.symbol].filter(
                    (s): s is string => !!s,
                  ),
                )}
              </p>
              <p>{add.unpriceableNext(bot)}</p>
              <div>
                <Button variant="primary" onClick={onOpenBot}>
                  {add.openBot}
                </Button>
              </div>
            </div>
          </Banner>
        )}

        {/* The new corridor has no side it can quote yet. Not a skip and not a
            warning to click past: the flow ends at a live corridor, so it waits
            here and starts on its own the moment money lands. */}
        {waitingForMoney && funding && address && (
          <AddressBlock funding={funding} address={address} />
        )}

        {waitingForMoney && funding && (
          <ul className="divide-y divide-line-soft rounded-lg border border-line-soft">
            {[softRow, stableRow]
              .filter((t): t is FundingToken => t !== null)
              .map((t) => (
                <TokenRow key={t.token} token={t} />
              ))}
            <GasRow funding={funding} pill={false} />
          </ul>
        )}

        {needs.length > 0 && (
          <ul className="list-disc space-y-0.5 pl-5 text-sm text-muted">
            {needs.map((n) => (
              <li key={n}>{n}</li>
            ))}
          </ul>
        )}

        {waitingForMoney && oneSided && symbols && (
          <p className="text-sm text-muted">
            {add.oneSided(bot, symbols.base, symbols.quote)}
          </p>
        )}

        {fundingError && <Banner tone="warning">{add.fundingUnreadable(fundingError)}</Banner>}

        {waitingForMoney && (
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p className="flex items-center gap-2 text-xs text-faint">
              {!funding && <Spinner />}
              {checkedAt
                ? fund.status(POLL_MS / 1000, formatClock(checkedAt))
                : fund.statusFirst}
            </p>
            <Button
              variant="ghost"
              busy={checking}
              onClick={() => {
                setChecking(true)
                void readFunding().finally(
                  () => mountedRef.current && setChecking(false),
                )
              }}
            >
              {fund.checkNow}
            </Button>
          </div>
        )}

        {/* The runner's own failures, with the same recovery the Fund step
            offers. Approve is refused while something else could send from this
            wallet, and the panel names what to stop. */}
        {failure && failure.stage === 'approve' && failure.blockedBy && (
          <Banner tone="warning">
            <div className="space-y-2">
              <p>{failure.message}</p>
              <Button
                variant="secondary"
                busy={stopping}
                onClick={() => void stopBlocking(failure.blockedBy ?? bot)}
              >
                {progressCopy.stopBot(failure.blockedBy)}
              </Button>
              {stopError && <p className="text-xs">{stopError}</p>}
            </div>
          </Banner>
        )}

        {failure && failure.stage === 'approve' && !failure.blockedBy && (
          <Banner tone="danger">
            <p>{progressCopy.approveFailed(failure.message)}</p>
            <p className="mt-1 text-xs opacity-80">
              {progressCopy.approveFailedHint(gasSymbol)}
            </p>
          </Banner>
        )}

        {failure && failure.stage === 'start' && (
          <Banner tone="danger">
            <p>{progressCopy.startFailed(failure.message)}</p>
            {failure.needsRecreate && (
              <p className="mt-1 text-xs opacity-80">{progressCopy.recreate}</p>
            )}
          </Banner>
        )}

        {failure && failure.stage === 'verify' && (
          <Banner tone="danger">
            <p>{progressCopy.verifyFailed}</p>
            {failure.logTail && (
              <p className="mt-1 whitespace-pre-wrap break-all font-mono text-xs opacity-80">
                {failure.logTail}
              </p>
            )}
          </Banner>
        )}

        {failure && (
          <div className="flex justify-end">
            <Button variant="primary" onClick={() => runner.retry()}>
              {progressCopy.retry}
            </Button>
          </div>
        )}
      </div>
    </Card>
  )
}

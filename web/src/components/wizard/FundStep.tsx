// The wizard's Approve step: gas in, approvals out, then start without
// another click.
//
// Approval is permission, not money: one `approve(Permit2)` per token the bot
// quotes, paid from gas, so Textile can settle a trade from this wallet
// against an order the bot signed. It needs no token balance, only gas, so
// this screen asks for gas alone. The screen polls GET /funding until the
// server's gate passes (gas covers the approvals still outstanding), then
// hands over to the shared start runner: approve on chain, check the Textile
// seats, start, confirm it stays up. The trading money is the bot page's
// business: the bot runs empty until it lands, and quotes the moment one side
// does.
//
// Nothing about progress is kept in the browser. On a reload the step asks the
// server again and lands where it should: a running bot goes straight through,
// an approved-but-stopped one starts, a gasless one shows the checklist.

import { useCallback, useEffect, useReducer, useRef, useState } from 'react'
import { ApiError, api } from '../../api'
import { formatAmount, formatClock } from '../../format'
import { LEVEL_CLASS } from '../../logBuffer'
import { Banner, Button, Card, Spinner } from '../ui'
import ProgressList, { type ProgressRow } from './ProgressList'
import { AddressBlock, ApprovalRow, GasRow, orderedTokens } from './FundingRows'
import { INITIAL_FUND, gateReasons, reduceFund } from './fundMachine'
import { errorText, useStartSequence, type StartOutcome } from './useStartSequence'
import { fund, progress as progressCopy } from './wizardCopy'
import type { LogLevel } from '../../types'

/** What the step reports when it is finished. Re-exported for the wizard. */
export type FundOutcome = StartOutcome

export interface FundStepProps {
  /** The created bot's name. */
  bot: string
  /**
   * Called exactly once when this step is finished. `live`: the bot is running.
   * `waiting`: funded and approved on chain, the operator's email is not
   * confirmed yet (the panel refuses Start until then). `rejected`: Textile
   * blocked the maker. A caller that only wants to move on can ignore the
   * argument.
   */
  onStarted: (outcome: FundOutcome) => void
  /**
   * Forget this bot and take the wizard back to its first step. The only
   * control that can break a step the operator is stuck on: there is no Back
   * from here, because the wizard ends at a live bot or at one waiting on a
   * confirmation, never at "I'll do it later". Not a way out of the wizard:
   * it starts the wizard again, and the bot it forgets keeps its wallet, its
   * money and its Textile request.
   */
  onStartOver: () => void
}

/** Default floors for the intro before the first read arrives. */
const DEFAULT_MIN_GAS_USD = 1

export default function FundStep({ bot, onStarted, onStartOver }: FundStepProps) {
  const [state, dispatch] = useReducer(reduceFund, INITIAL_FUND)
  const startedRef = useRef(false)
  const autoRanRef = useRef(false)
  const mountedRef = useRef(true)
  const [showOutput, setShowOutput] = useState(false)
  const [stopping, setStopping] = useState(false)
  const [stopError, setStopError] = useState<string | null>(null)
  const [checkingNow, setCheckingNow] = useState(false)

  // Through a ref so a parent's inline arrow doesn't change the identity of
  // `report` on every render, which would restart the loading effect.
  const onStartedRef = useRef(onStarted)
  onStartedRef.current = onStarted
  const report = useCallback((outcome: FundOutcome) => {
    if (startedRef.current) return
    startedRef.current = true
    onStartedRef.current(outcome)
  }, [])

  const runner = useStartSequence(bot, {
    onFunding: (funding) => dispatch({ type: 'funding', funding, at: Date.now() }),
    onOutcome: report,
  })
  const { run: startRun, reset: resetRun } = runner

  useEffect(() => {
    mountedRef.current = true
    return () => {
      mountedRef.current = false
    }
  }, [])

  /** Hand over to the runner once, per pass of the gate. */
  const triggerRun = useCallback(() => {
    if (autoRanRef.current) return
    autoRanRef.current = true
    dispatch({ type: 'run' })
    startRun()
  }, [startRun])

  // First read: the bot and its wallet together. A bot that is already running
  // goes straight through; one with a live process that is not quoting
  // (restarting, paused) is handed to the runner, which sorts it out from the
  // chain and the config.
  useEffect(() => {
    if (state.phase !== 'loading') return
    let cancelled = false
    void (async () => {
      try {
        const [current, funding] = await Promise.all([api.bot(bot), api.funding(bot)])
        if (cancelled) return
        dispatch({ type: 'loaded', bot: current, funding, at: Date.now() })
        if (current.running) {
          report({ kind: 'live' })
        } else if (current.canStop || funding.gate.passes) {
          triggerRun()
        }
      } catch (e) {
        if (cancelled) return
        if (e instanceof ApiError && e.status === 404) dispatch({ type: 'gone' })
        else if (e instanceof ApiError && e.status === 409) {
          dispatch({ type: 'unreadable', message: e.message })
        } else dispatch({ type: 'fetch-failed', message: errorText(e), at: Date.now() })
      }
    })()
    return () => {
      cancelled = true
    }
  }, [state.phase, bot, report, triggerRun])

  /** One wallet read. Used by the poll and by Check now. */
  const refresh = useCallback(async () => {
    try {
      const funding = await api.funding(bot)
      if (!mountedRef.current) return
      dispatch({ type: 'funding', funding, at: Date.now() })
      if (funding.gate.passes) triggerRun()
    } catch (e) {
      if (!mountedRef.current) return
      if (e instanceof ApiError && e.status === 404) dispatch({ type: 'gone' })
      else dispatch({ type: 'fetch-failed', message: errorText(e), at: Date.now() })
    }
  }, [bot, triggerRun])

  // The poll. Stops the moment the runner takes over (it reads funding itself)
  // and while a load is in flight.
  useEffect(() => {
    if (state.phase !== 'checking') return
    const timer = window.setInterval(() => void refresh(), state.pollMs)
    return () => clearInterval(timer)
  }, [state.phase, state.pollMs, refresh])

  async function checkNow() {
    setCheckingNow(true)
    await refresh()
    if (mountedRef.current) setCheckingNow(false)
  }

  function retry() {
    autoRanRef.current = false
    startedRef.current = false
    setStopError(null)
    resetRun()
    dispatch({ type: 'retry' })
  }

  async function stopBlocking(target: string) {
    setStopping(true)
    setStopError(null)
    try {
      await api.stop(target)
      if (!mountedRef.current) return
      retry()
    } catch (e) {
      if (!mountedRef.current) return
      setStopError(errorText(e))
    } finally {
      if (mountedRef.current) setStopping(false)
    }
  }

  const funding = state.funding
  const minGas = funding?.gate.minGasUsd ?? DEFAULT_MIN_GAS_USD
  const gasSymbol = funding?.gas.symbol ?? 'gas'
  // The same figure the gas row shows, so the title and the row never differ.
  const gasPrice = funding?.gas.price ?? null
  const gasAmount =
    gasPrice !== null && gasPrice > 0 ? formatAmount(String(minGas / gasPrice), 3) : null
  const title = fund.title(gasAmount, gasSymbol)
  const seq = runner.state
  const failure = seq.failure
  const busy = runner.active
  const finished = seq.stage === 'done' && seq.outcome !== null

  if (state.phase === 'gone') {
    return (
      <Card title={title}>
        <div className="space-y-4">
          <Banner tone="danger">{fund.gone}</Banner>
          <div className="flex justify-between">
            <Button onClick={onStartOver}>{fund.startOver}</Button>
          </div>
        </div>
      </Card>
    )
  }

  // A 409: the panel can't edit this bot's config, which is permanent, not a
  // hiccup. Nothing here can be funded or started, so the only useful control
  // is the one the `gone` branch offers: drop the resume record and set a bot
  // up again. Without it a stale record reopened this same dead screen on
  // every later visit to /add, and the only way out was to leave the wizard.
  if (state.phase === 'unreadable') {
    return (
      <Card title={title}>
        <div className="space-y-4">
          <Banner tone="danger">{fund.unreadable(state.loadError ?? '')}</Banner>
          <div className="flex flex-wrap items-center gap-3">
            <Button variant="primary" onClick={onStartOver}>
              {fund.startOver}
            </Button>
          </div>
        </div>
      </Card>
    )
  }

  const reasons = funding ? gateReasons(funding, fund) : []
  const address = funding?.operatorAddress ?? null

  const rows: ProgressRow[] = [
    {
      key: 'approve',
      title: progressCopy.approveTitle(seq.approveTokens, seq.progress.approve),
      sub: seq.progress.approve === 'skipped' ? undefined : progressCopy.approveSub,
      state: seq.progress.approve,
      children:
        seq.approveLines.length > 0 ? (
          <ApproveOutput
            lines={seq.approveLines}
            expanded={showOutput}
            onToggle={() => setShowOutput((v) => !v)}
          />
        ) : seq.stage === 'approve-busy' ? (
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
  ]

  return (
    <Card title={title}>
      <div className="space-y-4">
        {!funding && !state.loadError && (
          <div className="flex items-center gap-2 py-4 text-sm text-muted">
            <Spinner /> {fund.statusFirst}
          </div>
        )}

        {funding && address && <AddressBlock funding={funding} address={address} />}

        {/* No wallet address to show, so nothing can arrive and the gate can
            never pass for this bot. Same ending as the 409 above: start the
            wizard again rather than leave it. */}
        {funding && !address && (
          <Banner tone="warning">
            <div className="space-y-2">
              <p>{fund.noOperator}</p>
              <Button variant="secondary" onClick={onStartOver}>
                {fund.startOver}
              </Button>
            </div>
          </Banner>
        )}

        {funding && (
          <ul className="divide-y divide-line-soft rounded-lg border border-line-soft">
            <GasRow funding={funding} />
            {orderedTokens(funding)
              .filter((t) => t.approvalNeeded)
              .map((t) => (
                <ApprovalRow key={t.token} token={t} />
              ))}
          </ul>
        )}

        {funding && (
          <div className="space-y-1">
            <p className="text-sm">{fund.gate(funding.gate.minGasUsd, funding.gas.symbol)}</p>
            {state.phase === 'checking' && reasons.length > 0 && (
              <ul className="list-disc space-y-0.5 pl-5 text-sm text-muted">
                {reasons.map((r) => (
                  <li key={r}>{r}</li>
                ))}
              </ul>
            )}
            {(state.phase === 'running' || finished) && (
              <p className="text-sm font-bold text-success">{fund.fundsFound}</p>
            )}
          </div>
        )}

        {state.loadError && state.phase === 'checking' && (
          <Banner tone="warning">{fund.readError(state.loadError)}</Banner>
        )}
        {funding?.readError && !state.loadError && state.phase === 'checking' && (
          <Banner tone="warning">{fund.readError(funding.readError)}</Banner>
        )}

        {state.phase === 'checking' && (
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p className="text-xs text-faint">
              {state.lastCheckedAt
                ? fund.status(Math.round(state.pollMs / 1000), formatClock(state.lastCheckedAt))
                : fund.statusFirst}
            </p>
            <Button variant="ghost" busy={checkingNow} onClick={() => void checkNow()}>
              {fund.checkNow}
            </Button>
          </div>
        )}

        {(state.phase === 'running' || finished) && <ProgressList rows={rows} />}

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
            <p className="mt-1 text-xs opacity-80">{progressCopy.approveFailedHint(gasSymbol)}</p>
          </Banner>
        )}

        {/* Both failure banners carry their own evidence and their own control.
            Nothing links out to the bot page: following a link from here leaves
            a bot that is neither live nor waiting, which is the one ending the
            wizard must not have. */}
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
            {!failure.logTail && failure.message && (
              <p className="mt-1 text-xs opacity-80">{failure.message}</p>
            )}
          </Banner>
        )}

        {/* Back, a fresh start, and Retry. There is no way to leave the wizard
            from here: it ends at a running bot, or at the Waiting screen. The
            fresh start begins the wizard again rather than abandoning this bot,
            which keeps its money and its Textile request.

            It is offered ONLY where this step cannot finish on its own, which
            here means a failure the operator has already retried. On the
            ordinary waiting-for-money screen nothing is stuck, and a fresh
            start there would leave a created, funded, enrolled bot at neither
            ending, one orphan per press. The states that genuinely dead-end
            (gone, unreadable, no address) each carry their own start-over
            button above. */}
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex flex-wrap items-center gap-3">
            {!busy && failure && (
              <button
                type="button"
                className="text-xs text-muted underline"
                onClick={onStartOver}
              >
                {fund.differentBot}
              </button>
            )}
          </div>
          {failure && (
            <Button variant="primary" onClick={retry}>
              {fund.retry}
            </Button>
          )}
        </div>
      </div>
    </Card>
  )
}

/** The approve run's output: the last three lines, or all of them on request. */
function ApproveOutput({
  lines,
  expanded,
  onToggle,
}: {
  lines: { text: string; level: LogLevel }[]
  expanded: boolean
  onToggle: () => void
}) {
  const shown = expanded ? lines : lines.slice(-3)
  return (
    <div className="mt-2 space-y-1">
      <div
        className={`overflow-auto rounded-lg bg-canvas p-2 font-mono text-xs leading-relaxed ${
          expanded ? 'max-h-72' : 'max-h-24'
        }`}
      >
        {shown.map((line, i) => (
          <div key={i} className={`whitespace-pre-wrap break-all ${LEVEL_CLASS[line.level]}`}>
            {line.text}
          </div>
        ))}
      </div>
      {lines.length > 3 && (
        <button type="button" className="text-xs text-muted underline" onClick={onToggle}>
          {expanded ? progressCopy.hideOutput : progressCopy.showOutput}
        </button>
      )}
    </div>
  )
}

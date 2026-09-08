// The one runner that takes a funded bot to "running": approve spending on
// chain, check the Textile seats, start, and make sure it stays up.
//
// Shared by the Fund step (first run), the Confirm-your-email screen (once the
// address is confirmed) and the Live screen ("Start again"). Every stage re-derives its
// facts from the server rather than from what an earlier screen remembered:
// the chain says whether approvals are missing, the config says whether the
// bot may start, the bot's own state says whether it stayed up. That is what
// makes Retry safe to press at any point, and a reload mid-flow harmless.
//
// The work runs inside one effect keyed on a run id. Calling `run()` bumps the
// id; the effect's cleanup cancels the run in flight (React StrictMode's
// double mount included) and aborts the approve stream. Nothing is stored in
// the browser between runs.

import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError, api } from '../../api'
import { appendLine } from '../../logBuffer'
import { streamSse } from '../../sse'
import { progress as copy } from './wizardCopy'
import type { Bot, ExitEvent, Funding, LogLine, RfqStatusResult } from '../../types'

export type ProgressState = 'pending' | 'running' | 'done' | 'failed' | 'skipped'
export type StartStage = 'approve' | 'access' | 'start' | 'verify'
export type SequenceStage =
  | 'idle'
  | 'approving'
  | 'approve-busy'
  | 'verify-approval'
  | 'access'
  | 'starting'
  | 'start-busy'
  | 'verify-start'
  | 'done'
  | 'failed'

export interface StartFailure {
  stage: StartStage
  /** Server or chain text, shown inside a banner. */
  message: string
  /** Last error line from the bot's log, when the bot died right after start. */
  logTail: string | null
  /** Approve was refused because this bot must be stopped first. */
  blockedBy: string | null
  /** Start was refused because the container is gone. */
  needsRecreate: boolean
}

/**
 * How a run ended when it did not fail.
 *
 * `live`: the bot is running and stayed up. `waiting`: everything on our side
 * is done and the operator's unconfirmed email is what is missing (the panel
 * refuses Start until then). `rejected`: Textile blocked the maker.
 */
export type StartOutcome =
  | { kind: 'live' }
  | { kind: 'waiting'; status: RfqStatusResult | null; error: string | null }
  | { kind: 'rejected'; status: RfqStatusResult }

export interface StartSequenceState {
  stage: SequenceStage
  progress: Record<StartStage, ProgressState>
  /** Symbols the approve run is covering, for the row title. */
  approveTokens: string[]
  approveLines: LogLine[]
  failure: StartFailure | null
  outcome: StartOutcome | null
}

export interface StartSequenceHandlers {
  /** Every funding read the runner makes, so a checklist can refresh from it. */
  onFunding?: (funding: Funding) => void
  /** Fired once per run when it ends without failing. */
  onOutcome?: (outcome: StartOutcome) => void
}

export const IDLE_SEQUENCE: StartSequenceState = {
  stage: 'idle',
  progress: { approve: 'pending', access: 'pending', start: 'pending', verify: 'pending' },
  approveTokens: [],
  approveLines: [],
  failure: null,
  outcome: null,
}

/** approve-busy: 24 reads 5 s apart, about two minutes. */
const APPROVE_BUSY_TRIES = 24
/** start-busy: 12 tries 5 s apart, one minute. */
const START_BUSY_TRIES = 12
const BUSY_EVERY_MS = 5000
const FUNDING_RETRY_MS = 3000
const VERIFY_EVERY_MS = 2000
/** Consecutive `running` reads before the bot counts as up (about six seconds). */
const VERIFY_RUNNING_READS = 3
/** Panel read errors during verify are ignored this long: it may just be busy. */
const VERIFY_READ_GRACE_MS = 15_000
/** A bot neither running nor dead for this long is reported as it stands. */
const VERIFY_MAX_MS = 45_000
const LOG_TAIL_LINES = 40
const LOG_TAIL_WAIT_MS = 1500

export function errorText(e: unknown): string {
  return e instanceof ApiError ? e.message : String(e)
}

/** The panel's shared wallet lock, on approve and on start. */
export function isWalletBusy(message: string): boolean {
  return /wallet is busy/i.test(message)
}

function isConnectFirst(message: string): boolean {
  return /^Connect this bot to Textile/i.test(message)
}

function isNoContainer(message: string): boolean {
  return /no container/i.test(message)
}

/** Ends a run early: either a failure to show, or an outcome to report. */
class Halt {
  constructor(
    readonly failure: StartFailure | null,
    readonly outcome: StartOutcome | null,
  ) {}
}

const CANCELLED = Symbol('cancelled')

type ApproveStreamResult =
  | { kind: 'exit'; ok: boolean }
  | { kind: 'error'; message: string }
  | { kind: 'aborted' }

export function useStartSequence(bot: string, handlers: StartSequenceHandlers = {}) {
  const [state, setState] = useState<StartSequenceState>(IDLE_SEQUENCE)
  const [runId, setRunId] = useState(0)
  const handlersRef = useRef(handlers)
  handlersRef.current = handlers

  useEffect(() => {
    if (runId === 0) return
    let live = true
    let stream: { abort: () => void } | null = null
    const timers = new Set<number>()
    // The approve output as it streams, kept locally too so the failure text
    // can quote the last error line without reading React state.
    let approveLines: LogLine[] = []

    const check = () => {
      if (!live) throw CANCELLED
    }
    const update = (fn: (s: StartSequenceState) => StartSequenceState) => {
      if (live) setState(fn)
    }
    const setStage = (stage: SequenceStage) => update((s) => ({ ...s, stage }))
    const setProgress = (key: StartStage, value: ProgressState) =>
      update((s) => ({ ...s, progress: { ...s.progress, [key]: value } }))
    const sleep = (ms: number) =>
      new Promise<void>((resolve) => {
        const id = window.setTimeout(() => {
          timers.delete(id)
          resolve()
        }, ms)
        timers.add(id)
      })
    const fail = (stage: StartStage, message: string, extra: Partial<StartFailure> = {}): never => {
      throw new Halt(
        { stage, message, logTail: null, blockedBy: null, needsRecreate: false, ...extra },
        null,
      )
    }
    const finish = (outcome: StartOutcome): never => {
      throw new Halt(null, outcome)
    }

    /** One funding read, retried once after a short pause. */
    async function readFunding(): Promise<Funding> {
      try {
        return await api.funding(bot)
      } catch (first) {
        check()
        await sleep(FUNDING_RETRY_MS)
        check()
        try {
          return await api.funding(bot)
        } catch {
          throw first
        }
      }
    }

    function streamApprove(): Promise<ApproveStreamResult> {
      return new Promise((resolve) => {
        let settled = false
        const done = (result: ApproveStreamResult) => {
          if (settled) return
          settled = true
          stream = null
          resolve(result)
        }
        const sse = streamSse(
          api.approveUrl(bot),
          { method: 'POST' },
          {
            onEvent: (event, data) => {
              if (event === 'line') {
                approveLines = appendLine(approveLines, data as LogLine)
                const lines = approveLines
                update((s) => ({ ...s, approveLines: lines }))
              } else if (event === 'exit') {
                done({ kind: 'exit', ok: (data as ExitEvent).ok })
              } else if (event === 'error') {
                done({ kind: 'error', message: (data as { message: string }).message })
              }
            },
            // The stream closed without an exit event. The chain read that
            // follows decides what happened, so this is not a failure yet.
            onDone: () => done({ kind: 'exit', ok: false }),
            onError: (message) => done({ kind: 'error', message }),
          },
        )
        stream = {
          abort: () => {
            sse.abort()
            done({ kind: 'aborted' })
          },
        }
      })
    }

    /** The last error line from the bot's log, else its last line, else null. */
    function logTail(): Promise<string | null> {
      return new Promise((resolve) => {
        const lines: LogLine[] = []
        let settled = false
        let tail: { abort: () => void } | null = null
        let timer = 0
        const settle = () => {
          if (settled) return
          settled = true
          clearTimeout(timer)
          tail?.abort()
          const lastError = [...lines].reverse().find((l) => l.level === 'error')
          resolve((lastError ?? lines[lines.length - 1])?.text ?? null)
        }
        tail = streamSse(
          api.logsUrl(bot, LOG_TAIL_LINES),
          { method: 'GET' },
          {
            onEvent: (event, data) => {
              if (event === 'line') lines.push(data as LogLine)
            },
            onDone: settle,
            onError: settle,
          },
        )
        timer = window.setTimeout(settle, LOG_TAIL_WAIT_MS)
      })
    }

    interface Settled {
      /** RFQ is on in the config, so the bot may be started. */
      ready: boolean
      status: RfqStatusResult | null
      error: string | null
    }

    /**
     * Ask where this bot stands with Textile, and let the panel enable RFQ if
     * the answer is yes.
     *
     * The config is asked first on purpose: once a bot is seated, asking the
     * venue again rewrites the config and restarts a running bot. Only an
     * unseated bot gets the network call, and that call is also the thing that
     * writes `[rfq].enabled` when the operator's address is confirmed — which
     * is why it runs on the way out of a failed run too.
     */
    async function settleRfqStatus(): Promise<Settled> {
      setStage('access')
      setProgress('access', 'running')
      let ready = false
      try {
        ready = (await api.settings(bot, 0)).rfqEnabled
      } catch {
        ready = false
      }
      check()
      let status: RfqStatusResult | null = null
      let error: string | null = null
      if (!ready) {
        try {
          status = await api.checkRfqStatus(bot)
          check()
          ready = status.emailVerified && !!status.settings?.rfqEnabled
        } catch (e) {
          check()
          error = errorText(e)
        }
      }
      setProgress('access', 'done')
      return { ready, status, error }
    }

    /**
     * Fail, but settle the Textile side first.
     *
     * The chain half and the venue half are independent: a Permit2 approval
     * that could not be paid for says nothing about whether the operator's
     * address is confirmed. Halting on the approval without asking left a bot
     * whose email was confirmed sitting with RFQ still off in its config — the
     * one thing the onboarding is there to switch on. Skipping the wait for
     * funds made that the common ending rather than a rare one, so the run
     * asks on its way out and the config is right whenever the operator comes
     * back to press Start.
     *
     * Best effort by design: this is already a failure, and a second one on
     * the way out must not replace the message that explains it.
     */
    async function failSettling(
      stage: StartStage,
      message: string,
      extra: Partial<StartFailure> = {},
    ): Promise<never> {
      try {
        await settleRfqStatus()
      } catch (e) {
        if (e === CANCELLED) throw e
      }
      return fail(stage, message, extra)
    }

    async function execute(): Promise<void> {
      // 1. Approve spending, only if the chain says something is missing.
      setStage('approving')
      setProgress('approve', 'running')
      let funding: Funding
      try {
        funding = await readFunding()
      } catch (e) {
        check()
        return failSettling('approve', errorText(e))
      }
      check()
      handlersRef.current.onFunding?.(funding)
      const missing = funding.gate.approvalsMissing
      update((s) => ({ ...s, approveTokens: missing }))

      if (missing.length === 0) {
        setProgress('approve', 'skipped')
      } else {
        let current: Bot
        try {
          current = await api.bot(bot)
        } catch (e) {
          check()
          return failSettling('approve', errorText(e))
        }
        check()
        if (!current.canApprove) {
          return failSettling(
            'approve',
            current.approveBlockedReason ?? copy.approveBlocked,
            { blockedBy: current.approveBlockedBy },
          )
        }

        const result = await streamApprove()
        check()
        if (result.kind === 'error' && isWalletBusy(result.message)) {
          // Another approval holds the wallet. Wait for the chain to show it.
          setStage('approve-busy')
          let cleared = false
          for (let i = 0; i < APPROVE_BUSY_TRIES && !cleared; i++) {
            await sleep(BUSY_EVERY_MS)
            check()
            try {
              const again = await api.funding(bot)
              check()
              handlersRef.current.onFunding?.(again)
              cleared = again.gate.approvalsMissing.length === 0
            } catch {
              check()
            }
          }
          if (!cleared) return failSettling('approve', copy.approveStillBusy)
        } else {
          // Whatever the exit code, the chain is the truth: a transaction that
          // landed before the stream broke still counts.
          setStage('verify-approval')
          let after: Funding
          try {
            after = await readFunding()
          } catch (e) {
            check()
            return failSettling('approve', errorText(e))
          }
          check()
          handlersRef.current.onFunding?.(after)
          if (after.gate.approvalsMissing.length > 0) {
            const lastError = [...approveLines].reverse().find((l) => l.level === 'error')
            return failSettling(
              'approve',
              lastError?.text ??
                (result.kind === 'error' ? result.message : copy.approvalDidNotLand),
            )
          }
        }
        setProgress('approve', 'done')
      }

      // 2. The Textile seats. `settleRfqStatus` is what turns RFQ on in the
      // config when the venue says the address is confirmed, so every ending
      // of this run goes through it — the approve failures above reach it
      // through `failSettling` rather than halting before this point.
      const settled = await settleRfqStatus()
      check()
      if (!settled.ready) {
        if (settled.status?.enrollment?.flagged) {
          return finish({ kind: 'rejected', status: settled.status })
        }
        return finish({
          kind: 'waiting',
          status: settled.status,
          error: settled.error,
        })
      }

      // 3. Start. The panel refuses a bot whose wallet is held by another
      // action, so a busy answer is waited out rather than reported.
      setStage('starting')
      setProgress('start', 'running')
      let tries = 0
      for (;;) {
        try {
          await api.start(bot)
          check()
          break
        } catch (e) {
          check()
          const message = errorText(e)
          const status = e instanceof ApiError ? e.status : 0
          if (status === 400 && isConnectFirst(message)) {
            setProgress('start', 'pending')
            return finish({ kind: 'waiting', status: settled.status, error: null })
          }
          if (status === 409 && isWalletBusy(message)) {
            tries++
            if (tries >= START_BUSY_TRIES) return fail('start', copy.startStayedBusy)
            setStage('start-busy')
            await sleep(BUSY_EVERY_MS)
            check()
            setStage('starting')
            continue
          }
          if (status === 409 && isNoContainer(message)) {
            return fail('start', message, { needsRecreate: true })
          }
          return fail('start', message)
        }
      }
      setProgress('start', 'done')

      // 4. Verify it stays up. The panel can report running before the bot's
      // own preflight fails, so one read is not enough.
      setStage('verify-start')
      setProgress('verify', 'running')
      const startedAt = Date.now()
      let runningReads = 0
      for (;;) {
        await sleep(VERIFY_EVERY_MS)
        check()
        let current: Bot
        try {
          current = await api.bot(bot)
        } catch (e) {
          check()
          if (Date.now() - startedAt > VERIFY_READ_GRACE_MS) {
            return fail('verify', copy.verifyUnconfirmed(errorText(e)))
          }
          continue
        }
        check()
        if (current.state === 'running') {
          runningReads++
          if (runningReads >= VERIFY_RUNNING_READS) break
          continue
        }
        runningReads = 0
        if (
          current.state === 'exited' ||
          current.state === 'dead' ||
          current.state === 'restarting'
        ) {
          const tail = await logTail()
          check()
          return fail('verify', current.status, { logTail: tail })
        }
        if (Date.now() - startedAt > VERIFY_MAX_MS) return fail('verify', current.status)
      }
      setProgress('verify', 'done')
    }

    void (async () => {
      try {
        await execute()
        check()
        const outcome: StartOutcome = { kind: 'live' }
        update((s) => ({ ...s, stage: 'done', outcome }))
        handlersRef.current.onOutcome?.(outcome)
      } catch (e) {
        if (e === CANCELLED || !live) return
        if (e instanceof Halt) {
          const { failure, outcome } = e
          if (failure) {
            update((s) => ({
              ...s,
              stage: 'failed',
              failure,
              progress: { ...s.progress, [failure.stage]: 'failed' },
            }))
          } else if (outcome) {
            update((s) => ({ ...s, stage: 'done', outcome }))
            handlersRef.current.onOutcome?.(outcome)
          }
          return
        }
        // Not one of ours: a bug or a thrown non-Error. Show it rather than hang.
        const failure: StartFailure = {
          stage: 'start',
          message: errorText(e),
          logTail: null,
          blockedBy: null,
          needsRecreate: false,
        }
        update((s) => ({ ...s, stage: 'failed', failure }))
      }
    })()

    return () => {
      live = false
      stream?.abort()
      timers.forEach((id) => clearTimeout(id))
    }
  }, [runId, bot])

  // Leaving the page mid-approve kills the approve process on the desktop
  // runtime. A transaction already broadcast still lands; the chain read on
  // the next Retry picks it up. Worth a "are you sure?" all the same.
  useEffect(() => {
    if (state.stage !== 'approving') return
    const warn = (e: BeforeUnloadEvent) => {
      e.preventDefault()
      e.returnValue = ''
    }
    window.addEventListener('beforeunload', warn)
    return () => window.removeEventListener('beforeunload', warn)
  }, [state.stage])

  /** Start (or start over). Safe to call while a run is in flight: it replaces it. */
  const run = useCallback(() => {
    setState(IDLE_SEQUENCE)
    setRunId((n) => n + 1)
  }, [])

  /** Back to idle, cancelling any run in flight. */
  const reset = useCallback(() => {
    setState(IDLE_SEQUENCE)
    setRunId(0)
  }, [])

  const active =
    state.stage !== 'idle' && state.stage !== 'done' && state.stage !== 'failed'

  return { state, active, run, retry: run, reset }
}

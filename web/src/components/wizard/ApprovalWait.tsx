// The Waiting screen: funded and approved on chain, Textile's access decision
// outstanding. Polls the panel's access-status every 30 s (backing off to two
// minutes on venue errors) and, the moment Textile says yes, runs the shared
// start runner so the bot goes live without another click.
//
// One rule above all: access-status is never called again once the bot is
// seated. On an APPROVED bot that call rewrites the config and restarts a
// running bot. So the screen asks the config first (`settings.rfqEnabled`) and
// stops polling on the first APPROVED answer.

import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError, api } from '../../api'
import { formatClock } from '../../format'
import { Banner, Button, Card } from '../ui'
import ProgressList, { type ProgressRow } from './ProgressList'
import { errorText, useStartSequence } from './useStartSequence'
import { CONTACT_EMAIL, progress as progressCopy, wait } from './wizardCopy'
import type { RfqAccessResult } from '../../types'

export interface ApprovalWaitProps {
  bot: string
  /** Textile approved this maker and the bot is running. */
  onApproved: () => void
  /** The last access result the wizard saw, when it has one. */
  initial?: RfqAccessResult | null
  /**
   * Why the wizard has no access result: the panel could not reach Textile on
   * the way here. Shown at once, so the first thing an operator reads is that
   * the venue was away, not the "we are reviewing your request" copy.
   */
  initialError?: string | null
  /** Contact details still in the wizard's memory, for "Request access again". */
  contact?: { email: string; whatsapp: string }
  /**
   * Back to the Connect step (offered when the bot has no Textile credential).
   * The only way off this screen other than Textile answering: waiting for
   * approval is one of the wizard's two endings, so it offers no way out.
   */
  onBack?: () => void
}

const CHECK_MS = 30_000
const CHECK_MAX_MS = 120_000

type View =
  | 'pending'
  | 'rejected'
  | 'flagged'
  | 'approved-not-quotable'
  | 'restart-needed'
  | 'starting'
  | 'start-failed'

export default function ApprovalWait({
  bot,
  onApproved,
  initial = null,
  initialError = null,
  contact,
  onBack,
}: ApprovalWaitProps) {
  const seatedInitially =
    initial?.accessStatus === 'APPROVED' && !!initial.settings?.rfqEnabled
  const [access, setAccess] = useState<RfqAccessResult | null>(initial)
  const [approved, setApproved] = useState(seatedInitially)
  const [restartError, setRestartError] = useState<string | null>(
    seatedInitially ? (initial?.restartError ?? null) : null,
  )
  const [checking, setChecking] = useState(false)
  const [checkError, setCheckError] = useState<string | null>(initialError)
  const [keyMissing, setKeyMissing] = useState(false)
  const [lastCheckedAt, setLastCheckedAt] = useState<number | null>(null)
  const [nextCheckMs, setNextCheckMs] = useState(CHECK_MS)
  const [armed, setArmed] = useState(0)
  const [requesting, setRequesting] = useState(false)
  const [requestNote, setRequestNote] = useState<string | null>(null)
  const [restarting, setRestarting] = useState(false)
  const approvedRef = useRef(seatedInitially)
  const mountedRef = useRef(true)

  const runner = useStartSequence(bot, {
    onOutcome: (outcome) => {
      if (outcome.kind === 'live') onApproved()
    },
  })
  const { run: startRun } = runner

  useEffect(() => {
    mountedRef.current = true
    return () => {
      mountedRef.current = false
    }
  }, [])

  /** Textile seated the maker: stop polling and start the bot. */
  const seated = useCallback(
    (result: RfqAccessResult | null) => {
      approvedRef.current = true
      setApproved(true)
      if (result?.restartError) setRestartError(result.restartError)
      else startRun()
    },
    [startRun],
  )

  const check = useCallback(
    async (manual: boolean) => {
      if (approvedRef.current) return
      setChecking(true)
      try {
        const result = await api.checkRfqAccess(bot)
        if (!mountedRef.current || approvedRef.current) return
        setAccess(result)
        setCheckError(null)
        setKeyMissing(false)
        setLastCheckedAt(Date.now())
        setNextCheckMs(CHECK_MS)
        if (result.accessStatus === 'APPROVED' && result.settings?.rfqEnabled) {
          seated(result)
        }
      } catch (e) {
        if (!mountedRef.current) return
        const message = errorText(e)
        setLastCheckedAt(Date.now())
        setCheckError(message)
        if (e instanceof ApiError && e.status === 400 && /Connect to Textile first/i.test(message)) {
          setKeyMissing(true)
        } else {
          setNextCheckMs((ms) => (manual ? CHECK_MS : Math.min(ms * 2, CHECK_MAX_MS)))
        }
      } finally {
        if (mountedRef.current) {
          setChecking(false)
          setArmed((n) => n + 1)
        }
      }
    },
    [bot, seated],
  )

  const restartErrorRef = useRef(restartError)
  restartErrorRef.current = restartError

  // On mount: a bot already seated on RFQ (per its config) goes straight to
  // the start runner; anything else asks Textile once, then on the timer.
  useEffect(() => {
    let cancelled = false
    void (async () => {
      if (approvedRef.current) {
        if (!restartErrorRef.current) startRun()
        return
      }
      let enabled = false
      try {
        enabled = (await api.settings(bot, 0)).rfqEnabled
      } catch {
        enabled = false
      }
      if (cancelled) return
      if (enabled) {
        seated(null)
        return
      }
      void check(false)
    })()
    return () => {
      cancelled = true
    }
  }, [bot, check, seated, startRun])

  // The timer. Re-armed after every completed check; stopped once approved,
  // and while the start runner is working. That last guard is the module's
  // rule, not tidiness: a run started from this screen seats the bot, and
  // asking access-status again while it does rewrites the config and restarts
  // the bot underneath it.
  useEffect(() => {
    if (approved || checking || runner.active) return
    const timer = window.setTimeout(() => void check(false), nextCheckMs)
    return () => clearTimeout(timer)
  }, [approved, checking, runner.active, nextCheckMs, armed, check])

  // A run that ended without the bot going live. It carries Textile's answer,
  // so fold that back in and drop the "approved" latch: the bot is not seated
  // after all, the screen has to go back to showing what the venue said, and
  // Check again has to work again. Safe on the poll rule: a run that did not
  // reach 'live' left the bot stopped, so a later access-status call has no
  // running bot to restart.
  const outcome = runner.state.outcome
  useEffect(() => {
    if (!outcome || outcome.kind === 'live') return
    approvedRef.current = false
    setApproved(false)
    if (outcome.access) setAccess(outcome.access)
    setCheckError(outcome.kind === 'waiting' ? outcome.error : null)
  }, [outcome])

  function checkNow() {
    setNextCheckMs(CHECK_MS)
    void check(true)
  }

  async function requestAgain() {
    if (!contact?.email.trim()) return
    setRequesting(true)
    setRequestNote(null)
    try {
      const result = await api.requestRfqAccess(bot, {
        contactEmail: contact.email.trim(),
        contactWhatsapp: contact.whatsapp.trim() || undefined,
      })
      if (!mountedRef.current) return
      setAccess(result)
      setRequestNote(result.message)
    } catch (e) {
      if (!mountedRef.current) return
      setRequestNote(errorText(e))
    } finally {
      if (mountedRef.current) setRequesting(false)
    }
  }

  async function restartNow() {
    setRestarting(true)
    try {
      await api.restart(bot)
      if (!mountedRef.current) return
      setRestartError(null)
      startRun()
    } catch (e) {
      if (!mountedRef.current) return
      setRestartError(errorText(e))
    } finally {
      if (mountedRef.current) setRestarting(false)
    }
  }

  const seq = runner.state
  const slug = access?.enrollment?.makerSlug ?? null
  // A finished run that did not go live counts as no run at all: the screen
  // goes back to Textile's answer with its real copy and its own buttons,
  // instead of freezing on "Approved. Starting the bot" with no controls.
  const endedNotLive = seq.stage === 'done' && seq.outcome !== null && seq.outcome.kind !== 'live'
  // Flagged is tested before APPROVED on purpose: the panel answers a flagged
  // maker with APPROVED and flagged together, so an ordinary flagged maker
  // used to land on 'approved-not-quotable' and never see the one screen that
  // helps them, the one with the maker id and the way to reach Textile.
  const view: View =
    seq.stage === 'failed'
      ? 'start-failed'
      : seq.stage !== 'idle' && !endedNotLive
        ? 'starting'
        : approved && !endedNotLive
          ? restartError
            ? 'restart-needed'
            : 'starting'
          : access?.enrollment?.flagged
            ? 'flagged'
            : access?.accessStatus === 'APPROVED'
              ? 'approved-not-quotable'
              : access?.accessStatus === 'REJECTED'
                ? 'rejected'
                : 'pending'

  const title = {
    pending: wait.title,
    rejected: wait.rejectedTitle,
    flagged: wait.flaggedTitle,
    'approved-not-quotable': wait.notQuotableTitle,
    'restart-needed': wait.restartTitle,
    starting: wait.approvedTitle,
    'start-failed': wait.startFailedTitle,
  }[view]

  const rows: ProgressRow[] = [
    {
      key: 'approve',
      title: progressCopy.approveTitle(seq.approveTokens, seq.progress.approve),
      state: seq.progress.approve,
    },
    { key: 'access', title: progressCopy.accessTitle, state: seq.progress.access },
    { key: 'start', title: progressCopy.startTitle, state: seq.progress.start },
    {
      key: 'verify',
      title: progressCopy.verifyTitle,
      sub: progressCopy.verifySub,
      state: seq.progress.verify,
    },
  ]

  const statusLine = checkError
    ? keyMissing
      ? null
      : wait.venueError(checkError, Math.round(nextCheckMs / 1000))
    : lastCheckedAt
      ? wait.status(formatClock(lastCheckedAt), Math.round(nextCheckMs / 1000))
      : wait.statusFirst

  const mailto = `mailto:${CONTACT_EMAIL}?subject=${encodeURIComponent(wait.emailSubject(slug))}`
  const failure = seq.failure

  return (
    <Card title={title}>
      <div className="space-y-4">
        {view === 'pending' && (
          <>
            <p className="text-sm text-muted">{wait.body}</p>
            {access?.message && <Banner tone="info">{access.message}</Banner>}
            {access?.emailVerified === false && (
              <Banner tone="warning">{wait.emailVerify}</Banner>
            )}
            {keyMissing && checkError && <Banner tone="danger">{checkError}</Banner>}
            <p className="text-sm">{wait.keepOpen}</p>
            {statusLine && <p className="text-xs text-faint">{statusLine}</p>}
            <div className="flex flex-wrap items-center gap-3">
              {!keyMissing && (
                <Button busy={checking} onClick={checkNow}>
                  {wait.checkNow}
                </Button>
              )}
              {keyMissing && onBack && (
                <Button variant="primary" onClick={onBack}>
                  {wait.backToConnect}
                </Button>
              )}
            </div>
          </>
        )}

        {(view === 'rejected' || view === 'flagged') && (
          <>
            <p className="text-sm">
              {view === 'rejected' ? wait.rejectedBody(slug) : wait.flaggedBody(slug)}
            </p>
            {access?.message && <Banner tone="danger">{access.message}</Banner>}
            <p className="text-sm text-muted">{wait.contact(slug)}</p>
            {requestNote && <Banner tone="info">{requestNote}</Banner>}
            {statusLine && <p className="text-xs text-faint">{statusLine}</p>}
            <div className="flex flex-wrap items-center gap-3">
              <a
                className="inline-flex items-center rounded-lg bg-accent px-3 py-1.5 text-sm font-bold text-on-accent hover:opacity-90"
                href={mailto}
              >
                {wait.emailTextile}
              </a>
              {view === 'rejected' && contact?.email.trim() && (
                <Button busy={requesting} onClick={() => void requestAgain()}>
                  {wait.requestAgain}
                </Button>
              )}
              <Button busy={checking} onClick={checkNow}>
                {wait.checkAgain}
              </Button>
            </div>
          </>
        )}

        {/* Textile says approved, the bot's config doesn't say so yet: the
            venue answering and the panel writing that answer, out of step for a
            moment. The wizard can settle it itself, so the main button runs the
            start sequence (which re-reads the config, asks the venue again if
            it has to, and starts the bot). No link to Settings: leaving here
            strands a bot that is neither live nor waiting. */}
        {view === 'approved-not-quotable' && (
          <>
            <p className="text-sm text-muted">{wait.notQuotableBody}</p>
            {/* The operator pressed Finish setting up and it came back here.
                Say so, rather than show the same screen as if nothing ran. */}
            {endedNotLive && <Banner tone="warning">{wait.notQuotableAgain}</Banner>}
            {access?.message && <Banner tone="warning">{access.message}</Banner>}
            {statusLine && <p className="text-xs text-faint">{statusLine}</p>}
            <div className="flex flex-wrap items-center gap-3">
              <Button variant="primary" onClick={() => startRun()}>
                {wait.finishSetup}
              </Button>
              <Button busy={checking} onClick={checkNow}>
                {wait.checkAgain}
              </Button>
            </div>
          </>
        )}

        {view === 'restart-needed' && (
          <>
            <p className="text-sm">{wait.restartBody}</p>
            {restartError && <Banner tone="warning">{restartError}</Banner>}
            <div className="flex flex-wrap items-center gap-3">
              <Button variant="primary" busy={restarting} onClick={() => void restartNow()}>
                {wait.restartNow}
              </Button>
            </div>
          </>
        )}

        {(view === 'starting' || view === 'start-failed') && (
          <>
            <p className="text-sm text-muted">{wait.approvedBody}</p>
            {access?.message && seq.stage === 'idle' && (
              <Banner tone="success">{access.message}</Banner>
            )}
            <ProgressList rows={rows} />
            {failure && failure.stage === 'approve' && (
              <Banner tone={failure.blockedBy ? 'warning' : 'danger'}>
                {failure.blockedBy ? failure.message : progressCopy.approveFailed(failure.message)}
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
            {/* The log tail is inlined rather than linked: this screen is one
                of the wizard's two endings and has no way out of the flow. */}
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
            {failure && (
              <div className="flex flex-wrap items-center gap-3">
                <Button variant="primary" onClick={runner.retry}>
                  {progressCopy.retry}
                </Button>
              </div>
            )}
          </>
        )}
      </div>
    </Card>
  )
}

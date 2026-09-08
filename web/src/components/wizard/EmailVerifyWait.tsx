// The Confirm-your-email screen: funded and approved on chain, the operator's
// address still unconfirmed. Polls the panel's rfq/status every 30 s (backing
// off to two minutes on venue errors) and, the moment Textile reports the
// address confirmed, runs the shared start runner so the bot goes live without
// another click.
//
// One rule above all: rfq/status is never called again once the bot is seated.
// On a seated bot that call rewrites the config and restarts a running bot. So
// the screen asks the config first (`settings.rfqEnabled`) and stops polling on
// the first confirmed answer.

import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError, api } from '../../api'
import { formatClock } from '../../format'
import { Banner, Button, Card, Field, Input } from '../ui'
import ProgressList, { type ProgressRow } from './ProgressList'
import { errorText, useStartSequence } from './useStartSequence'
import { CONTACT_EMAIL, progress as progressCopy, wait } from './wizardCopy'
import type { RfqStatusResult } from '../../types'

export interface EmailVerifyWaitProps {
  bot: string
  /** The address is confirmed and the bot is running. */
  onApproved: () => void
  /** The last status the wizard saw, when it has one. */
  initial?: RfqStatusResult | null
  /**
   * Why the wizard has no status: the panel could not reach Textile on the way
   * here. Shown at once, so the first thing an operator reads is that the venue
   * was away, not the "check your inbox" copy.
   */
  initialError?: string | null
  /** The address still in the wizard's memory, for Resend. */
  contactEmail?: string
  /**
   * Back to the Connect step (offered when the bot has no Textile credential).
   * The only way off this screen other than confirming: this is one of the
   * wizard's two endings, so it offers no way out.
   */
  onBack?: () => void
}

const CHECK_MS = 30_000
const CHECK_MAX_MS = 120_000

/** Same bar the venue applies, so a typo stops here rather than at the venue. */
const isEmail = (s: string) => /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(s.trim())

type View =
  | 'unconfirmed'
  | 'flagged'
  | 'confirmed-not-quotable'
  | 'restart-needed'
  | 'starting'
  | 'start-failed'

export default function EmailVerifyWait({
  bot,
  onApproved,
  initial = null,
  initialError = null,
  contactEmail,
  onBack,
}: EmailVerifyWaitProps) {
  const seatedInitially = !!initial?.emailVerified && !!initial.settings?.rfqEnabled
  const [status, setStatus] = useState<RfqStatusResult | null>(initial)
  const [seatedNow, setSeatedNow] = useState(seatedInitially)
  const [restartError, setRestartError] = useState<string | null>(
    seatedInitially ? (initial?.restartError ?? null) : null,
  )
  const [checking, setChecking] = useState(false)
  const [checkError, setCheckError] = useState<string | null>(initialError)
  const [keyMissing, setKeyMissing] = useState(false)
  const [lastCheckedAt, setLastCheckedAt] = useState<number | null>(null)
  const [nextCheckMs, setNextCheckMs] = useState(CHECK_MS)
  const [armed, setArmed] = useState(0)
  const [resending, setResending] = useState(false)
  const [resendNote, setResendNote] = useState<string | null>(null)
  const [typedEmail, setTypedEmail] = useState('')
  const [restarting, setRestarting] = useState(false)
  const seatedRef = useRef(seatedInitially)
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
    (result: RfqStatusResult | null) => {
      seatedRef.current = true
      setSeatedNow(true)
      if (result?.restartError) setRestartError(result.restartError)
      else startRun()
    },
    [startRun],
  )

  const check = useCallback(
    async (manual: boolean) => {
      if (seatedRef.current) return
      setChecking(true)
      try {
        const result = await api.checkRfqStatus(bot)
        if (!mountedRef.current || seatedRef.current) return
        setStatus(result)
        setCheckError(null)
        setKeyMissing(false)
        setLastCheckedAt(Date.now())
        setNextCheckMs(CHECK_MS)
        if (result.emailVerified && result.settings?.rfqEnabled) seated(result)
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
      if (seatedRef.current) {
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

  // The timer. Re-armed after every completed check; stopped once seated, and
  // while the start runner is working. That last guard is the module's rule,
  // not tidiness: a run started from this screen seats the bot, and asking
  // rfq/status again while it does rewrites the config and restarts the bot
  // underneath it.
  useEffect(() => {
    if (seatedNow || checking || runner.active) return
    const timer = window.setTimeout(() => void check(false), nextCheckMs)
    return () => clearTimeout(timer)
  }, [seatedNow, checking, runner.active, nextCheckMs, armed, check])

  // A run that ended without the bot going live. It carries Textile's answer,
  // so fold that back in and drop the "seated" latch: the bot is not seated
  // after all, the screen has to go back to showing what the venue said, and
  // Check again has to work again. Safe on the poll rule: a run that did not
  // reach 'live' left the bot stopped, so a later rfq/status call has no
  // running bot to restart.
  const outcome = runner.state.outcome
  useEffect(() => {
    if (!outcome || outcome.kind === 'live') return
    seatedRef.current = false
    setSeatedNow(false)
    if (outcome.status) setStatus(outcome.status)
    setCheckError(outcome.kind === 'waiting' ? outcome.error : null)
  }, [outcome])

  function checkNow() {
    setNextCheckMs(CHECK_MS)
    void check(true)
  }

  /**
   * Send (or resend) the confirmation link. `address` is what the operator
   * typed when the bot has none on file — an older bot that connected before
   * this step existed, or a run whose first send was refused. Without it this
   * screen is a dead end: nothing to resend, and AddCorridorFlow mounts it
   * with no way back either.
   */
  async function sendLink(address: string) {
    const email = address.trim()
    if (!isEmail(email)) return
    setResending(true)
    setResendNote(null)
    try {
      const result = await api.verifyRfqEmail(bot, { contactEmail: email })
      if (!mountedRef.current) return
      setResendNote(result.message)
      setTypedEmail('')
      // Pick the address up on the next poll rather than trusting local state.
      void check(true)
    } catch (e) {
      if (!mountedRef.current) return
      setResendNote(errorText(e))
    } finally {
      if (mountedRef.current) setResending(false)
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
  const slug = status?.enrollment?.makerSlug ?? null
  const email = contactEmail?.trim() || status?.contactEmail?.trim() || null
  // A finished run that did not go live counts as no run at all: the screen
  // goes back to Textile's answer with its real copy and its own buttons,
  // instead of freezing on "Confirmed. Starting the bot" with no controls.
  const endedNotLive = seq.stage === 'done' && seq.outcome !== null && seq.outcome.kind !== 'live'
  // Flagged is tested before confirmed on purpose: the panel answers a blocked
  // maker with emailVerified and flagged together, so an ordinary blocked maker
  // used to land on 'confirmed-not-quotable' and never see the one screen that
  // helps them, the one with the maker id and the way to reach Textile.
  const view: View =
    seq.stage === 'failed'
      ? 'start-failed'
      : seq.stage !== 'idle' && !endedNotLive
        ? 'starting'
        : seatedNow && !endedNotLive
          ? restartError
            ? 'restart-needed'
            : 'starting'
          : status?.enrollment?.flagged
            ? 'flagged'
            : status?.emailVerified
              ? 'confirmed-not-quotable'
              : 'unconfirmed'

  const title = {
    unconfirmed: wait.title,
    flagged: wait.flaggedTitle,
    'confirmed-not-quotable': wait.notQuotableTitle,
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
        {view === 'unconfirmed' && (
          <>
            <p className="text-sm text-muted">{email ? wait.body : wait.noAddressBody}</p>
            {email && <Banner tone="info">{wait.sentTo(email)}</Banner>}
            {keyMissing && checkError && <Banner tone="danger">{checkError}</Banner>}
            {resendNote && <Banner tone="info">{resendNote}</Banner>}
            {/* No address on file, so there is nothing to resend and nothing to
                wait for. Ask for one here: this screen is one of the wizard's
                two endings and AddCorridorFlow mounts it with no Back. */}
            {!keyMissing && !email && (
              <Field label={wait.addressLabel} hint={wait.addressHint}>
                <Input
                  value={typedEmail}
                  inputMode="email"
                  placeholder="you@desk.com"
                  onChange={(e) => setTypedEmail(e.target.value)}
                />
              </Field>
            )}
            {email && <p className="text-sm">{wait.keepOpen}</p>}
            {statusLine && <p className="text-xs text-faint">{statusLine}</p>}
            <div className="flex flex-wrap items-center gap-3">
              {!keyMissing && !email && (
                <Button
                  variant="primary"
                  busy={resending}
                  disabled={!isEmail(typedEmail)}
                  onClick={() => void sendLink(typedEmail)}
                >
                  {wait.sendLink}
                </Button>
              )}
              {!keyMissing && (
                <Button busy={checking} onClick={checkNow}>
                  {wait.checkNow}
                </Button>
              )}
              {!keyMissing && email && (
                <Button busy={resending} onClick={() => void sendLink(email)}>
                  {wait.resend}
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

        {view === 'flagged' && (
          <>
            <p className="text-sm">{wait.flaggedBody(slug)}</p>
            {status?.message && <Banner tone="danger">{status.message}</Banner>}
            <p className="text-sm text-muted">{wait.contact(slug)}</p>
            {statusLine && <p className="text-xs text-faint">{statusLine}</p>}
            <div className="flex flex-wrap items-center gap-3">
              <a
                className="inline-flex items-center rounded-lg bg-accent px-3 py-1.5 text-sm font-bold text-on-accent hover:opacity-90"
                href={mailto}
              >
                {wait.emailTextile}
              </a>
              <Button busy={checking} onClick={checkNow}>
                {wait.checkAgain}
              </Button>
            </div>
          </>
        )}

        {/* Textile says confirmed, the bot's config doesn't say so yet: the
            venue answering and the panel writing that answer, out of step for a
            moment. The wizard can settle it itself, so the main button runs the
            start sequence (which re-reads the config, asks the venue again if
            it has to, and starts the bot). No link to Settings: leaving here
            strands a bot that is neither live nor waiting. */}
        {view === 'confirmed-not-quotable' && (
          <>
            <p className="text-sm text-muted">{wait.notQuotableBody}</p>
            {/* The operator pressed Finish setting up and it came back here.
                Say so, rather than show the same screen as if nothing ran. */}
            {endedNotLive && <Banner tone="warning">{wait.notQuotableAgain}</Banner>}
            {status?.message && <Banner tone="warning">{status.message}</Banner>}
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
            {status?.message && seq.stage === 'idle' && (
              <Banner tone="success">{status.message}</Banner>
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

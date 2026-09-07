// The last wizard screen: the bot is running. Proves it with a live quote from
// Textile's public preview, restricted to this bot's wallet, so the operator
// sees their own price the way a taker would. Keeps an eye on the bot (5 s
// poll) and offers "Start again" inline if it stops. Says the two things that
// keep it live: the app stays open, the machine stays awake.
//
// Never calls access-status: on a seated bot that rewrites the config and
// restarts it.

import { useCallback, useEffect, useRef, useState } from 'react'
import { Link, useNavigate } from 'react-router-dom'
import { ApiError, api } from '../../api'
import { botPath as botRoute } from '../../botRoutes'
import { formatAmount, formatRate } from '../../format'
import { Banner, Button, Card, Spinner, StatePill } from '../ui'
import ProgressList, { type ProgressRow } from './ProgressList'
import { clearResume } from './resume'
import { errorText, useStartSequence } from './useStartSequence'
import { PUBLIC_SWAP_BASE, live, progress as progressCopy } from './wizardCopy'
import type { Bot, Funding, PanelRuntime, QuoteDirection, QuoteProof } from '../../types'

export interface LiveStepProps {
  bot: string
  /**
   * The corridor as the wizard knows it, for the public swap link when the
   * quote proof can't supply one (a venue other than Textile's, or a proof that
   * hasn't answered yet). Null for a custom corridor.
   */
  corridor: { sellSymbol: string; buySymbol: string; chainId: number } | null
  /** Where "Open the bot page" goes. */
  botPath: string
  /** Overrides the navigation to `botPath`. */
  onOpenBot?: () => void
  /**
   * Drop the wizard's resume record on the way out. Default true: this is the
   * end of a run that created a bot, so the record has done its job.
   *
   * False on the add-a-corridor path. That path never wrote a record, and the
   * one in storage may belong to a different, half-created bot in another tab.
   */
  clearsResume?: boolean
}

/** Levels take a few seconds to publish after start. */
const FIRST_PROOF_MS = 3000
/** Automatic retries after a miss. About 100 s in total. */
const PROOF_RETRY_MS = [5000, 10_000, 20_000, 30_000, 30_000]
/** The venue's per-IP bucket is 60/min; a firm quote can hold the book. */
const PROOF_MIN_WAIT_MS = 5000
const MANUAL_THROTTLE_MS = 5000
const HEALTH_MS = 5000
/** Restricted misses before one open-market call is made for context. */
const OPEN_MARKET_AFTER_MISSES = 2

interface ProofError {
  status: number
  message: string
}

export default function LiveStep({
  bot,
  corridor,
  botPath,
  onOpenBot,
  clearsResume = true,
}: LiveStepProps) {
  const navigate = useNavigate()
  const [runtime, setRuntime] = useState<PanelRuntime | null>(null)
  const [funding, setFunding] = useState<Funding | null>(null)
  const [proof, setProof] = useState<QuoteProof | null>(null)
  const [openMarket, setOpenMarket] = useState<QuoteProof | null>(null)
  const [proofError, setProofError] = useState<ProofError | null>(null)
  const [fetching, setFetching] = useState(false)
  const [fetchedAt, setFetchedAt] = useState<number | null>(null)
  const [nextProofAt, setNextProofAt] = useState<number | null>(null)
  const [exhausted, setExhausted] = useState(false)
  const [health, setHealth] = useState<Bot | null>(null)
  const [healthError, setHealthError] = useState<string | null>(null)
  const [now, setNow] = useState(() => Date.now())

  const mountedRef = useRef(true)
  const timerRef = useRef<number | null>(null)
  const attemptsRef = useRef(0)
  const missesRef = useRef(0)
  const openMarketAskedRef = useRef(false)
  const lastManualRef = useRef(0)
  const directionRef = useRef<QuoteDirection>('usdtToSoft')
  const fetchProofRef = useRef<() => Promise<void>>(async () => {})

  const runner = useStartSequence(bot)

  const clearTimer = () => {
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current)
      timerRef.current = null
    }
  }

  const schedule = useCallback((delayMs: number) => {
    clearTimer()
    setNextProofAt(Date.now() + delayMs)
    timerRef.current = window.setTimeout(() => {
      timerRef.current = null
      void fetchProofRef.current()
    }, delayMs)
  }, [])

  const fetchOpenMarket = useCallback(
    async (direction: QuoteDirection) => {
      try {
        const res = await api.quoteProof(bot, { direction, onlyThisBot: false, pool: 0 })
        if (mountedRef.current && res.status === 'preview') setOpenMarket(res)
      } catch {
        // Context only. Nothing to show if it fails.
      }
    },
    [bot],
  )

  const fetchProof = useCallback(async () => {
    if (!mountedRef.current) return
    clearTimer()
    setNextProofAt(null)
    setFetching(true)
    const direction = directionRef.current
    let retry = false
    let retryAfter: number | null = null
    try {
      const res = await api.quoteProof(bot, { direction, onlyThisBot: true, pool: 0 })
      if (!mountedRef.current) return
      setProof(res)
      setProofError(null)
      setFetchedAt(Date.now())
      if (res.status === 'preview') {
        missesRef.current = 0
      } else {
        retry = true
        retryAfter = res.retryAfterMs
        if (res.reason === 'no_restricted_liquidity' || res.reason === 'no_makers_online') {
          missesRef.current++
          if (missesRef.current >= OPEN_MARKET_AFTER_MISSES && !openMarketAskedRef.current) {
            openMarketAskedRef.current = true
            void fetchOpenMarket(direction)
          }
        }
      }
    } catch (e) {
      if (!mountedRef.current) return
      const status = e instanceof ApiError ? e.status : 0
      setProofError({ status, message: errorText(e) })
      setFetchedAt(Date.now())
      // The panel maps a venue outage to 502; 0 is the panel itself being away.
      retry = status === 502 || status === 503 || status === 504 || status === 0
    } finally {
      if (mountedRef.current) setFetching(false)
    }
    if (!mountedRef.current || !retry) return
    const idx = attemptsRef.current
    const step = PROOF_RETRY_MS[idx]
    if (step === undefined) {
      setExhausted(true)
      return
    }
    attemptsRef.current = idx + 1
    schedule(retryAfter !== null ? Math.max(retryAfter, PROOF_MIN_WAIT_MS) : step)
  }, [bot, fetchOpenMarket, schedule])
  fetchProofRef.current = fetchProof

  // On mount: who runs the bots (for the keep-alive copy) and which side is
  // funded (for the proof's direction), then the first proof a few seconds in.
  useEffect(() => {
    mountedRef.current = true
    let cancelled = false
    const mountedAt = Date.now()
    void (async () => {
      const [session, fundingRead] = await Promise.allSettled([api.session(), api.funding(bot)])
      if (cancelled) return
      if (session.status === 'fulfilled') setRuntime(session.value.runtime)
      if (fundingRead.status === 'fulfilled') {
        setFunding(fundingRead.value)
        directionRef.current = directionFor(fundingRead.value)
      }
      schedule(Math.max(0, FIRST_PROOF_MS - (Date.now() - mountedAt)))
    })()
    return () => {
      cancelled = true
      mountedRef.current = false
      clearTimer()
    }
  }, [bot, schedule])

  // Bot health.
  useEffect(() => {
    let cancelled = false
    const tick = async () => {
      try {
        const current = await api.bot(bot)
        if (cancelled) return
        setHealth(current)
        setHealthError(null)
      } catch (e) {
        if (cancelled) return
        setHealthError(errorText(e))
      }
    }
    void tick()
    const timer = window.setInterval(() => void tick(), HEALTH_MS)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [bot])

  // A one-second clock for "fetched n s ago" and "trying again in n s".
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(timer)
  }, [])

  function tryAgain() {
    const t = Date.now()
    if (t - lastManualRef.current < MANUAL_THROTTLE_MS) return
    lastManualRef.current = t
    attemptsRef.current = 0
    setExhausted(false)
    void fetchProof()
  }

  function openBot() {
    if (clearsResume) clearResume()
    if (onOpenBot) onOpenBot()
    else navigate(botPath)
  }

  const stable = funding?.tokens.find((t) => t.role === 'stable')?.symbol ?? null
  const soft = funding?.tokens.find((t) => t.role === 'soft')?.symbol ?? null
  const pairLabel =
    proof?.pair.label ?? (soft && stable ? `${soft} / ${stable}` : null)
  const network = proof?.pair.networkLabel ?? funding?.networkLabel ?? null
  const unfunded =
    funding !== null && !funding.readError && funding.gate.fundedTokens.length === 0

  const fallbackSwapUrl = corridor
    ? `${PUBLIC_SWAP_BASE}?sell=${encodeURIComponent(corridor.sellSymbol)}&buy=${encodeURIComponent(corridor.buySymbol)}&chainId=${corridor.chainId}`
    : null
  const swapUrl = proof?.swapUrl ?? fallbackSwapUrl
  const mineUrl =
    proof?.fromThisBot === true
      ? (proof.swapUrlMine ??
        (fallbackSwapUrl && funding?.operatorAddress
          ? `${fallbackSwapUrl}&restricted=${encodeURIComponent(funding.operatorAddress)}`
          : null))
      : null

  const secondsUntil = nextProofAt !== null ? Math.max(0, Math.ceil((nextProofAt - now) / 1000)) : null
  const fetchedAgo = fetchedAt !== null ? Math.max(0, Math.round((now - fetchedAt) / 1000)) : null
  const stopped = health !== null && !health.running && !runner.active
  const seq = runner.state
  const failure = seq.failure

  // The recovery list shows while a restart runs and until the health poll
  // confirms the bot is back, then gets out of the way.
  const showRecovery = seq.stage !== 'idle' && !(seq.stage === 'done' && health?.running)
  const rows: ProgressRow[] = [
    {
      key: 'approve',
      title: progressCopy.approveTitle(seq.approveTokens, seq.progress.approve),
      state: seq.progress.approve,
    },
    { key: 'access', title: progressCopy.accessTitle, state: seq.progress.access },
    { key: 'start', title: progressCopy.startTitle, state: seq.progress.start },
    { key: 'verify', title: progressCopy.verifyTitle, sub: progressCopy.verifySub, state: seq.progress.verify },
  ]

  return (
    <Card
      title={
        <div className="flex items-center gap-3">
          <h2 className="text-base font-bold">{live.title}</h2>
          {health && <StatePill state={health.state} status={health.status} />}
        </div>
      }
    >
      <div className="space-y-4">
        <p className="text-lg font-bold">{live.headline(pairLabel, network)}</p>

        {healthError && <p className="text-xs text-warning">{live.panelUnreachable(healthError)}</p>}

        {stopped && health && (
          <Banner tone="danger">
            <div className="space-y-2">
              <p>{live.stopped(health.status)}</p>
              <div className="flex flex-wrap items-center gap-3">
                <Button variant="secondary" onClick={runner.run}>
                  {live.startAgain}
                </Button>
                <Link className="text-xs underline" to={botRoute(bot, 'logs')}>
                  {live.seeLogs}
                </Link>
              </div>
            </div>
          </Banner>
        )}

        {showRecovery && (
          <div className="space-y-3">
            <ProgressList rows={rows} />
            {failure && (
              <Banner tone="danger">
                <p>
                  {failure.stage === 'approve'
                    ? progressCopy.approveFailed(failure.message)
                    : failure.stage === 'start'
                      ? progressCopy.startFailed(failure.message)
                      : progressCopy.verifyFailed}
                </p>
                {failure.logTail && (
                  <p className="mt-1 whitespace-pre-wrap break-all font-mono text-xs opacity-80">
                    {failure.logTail}
                  </p>
                )}
                <div className="mt-2 flex items-center gap-3">
                  <Button variant="secondary" onClick={runner.retry}>
                    {progressCopy.retry}
                  </Button>
                  <Link className="text-xs underline" to={botRoute(bot, 'logs')}>
                    {progressCopy.seeLogs}
                  </Link>
                </div>
              </Banner>
            )}
          </div>
        )}

        <div className="rounded-lg border border-line-soft bg-canvas p-4">
          <ProofBlock
            proof={proof}
            openMarket={openMarket}
            error={proofError}
            fetching={fetching}
            fetchedAgo={fetchedAgo}
            secondsUntil={secondsUntil}
            exhausted={exhausted}
            unfunded={unfunded}
            onTryAgain={tryAgain}
            logsHref={botRoute(bot, 'logs')}
          />
        </div>

        {(swapUrl || mineUrl) && (
          <div className="flex flex-wrap items-center gap-4 text-sm">
            {swapUrl && (
              <a className="text-accent underline" href={swapUrl} target="_blank" rel="noreferrer">
                {live.swapLink}
              </a>
            )}
            {mineUrl && (
              <a className="text-accent underline" href={mineUrl} target="_blank" rel="noreferrer">
                {live.mineLink}
              </a>
            )}
          </div>
        )}

        <Banner tone="warning">
          <p className="font-bold">{live.keepHeader}</p>
          <ol className="mt-1 list-decimal space-y-0.5 pl-5">
            <li>{runtime === 'docker' ? live.keepDocker : live.keepProcess}</li>
            <li>{live.keepAwake}</li>
          </ol>
        </Banner>

        <div className="flex justify-end">
          <Button variant="primary" onClick={openBot}>
            {live.openBot}
          </Button>
        </div>
      </div>
    </Card>
  )
}

/**
 * Which side to prove. A funded soft token means the bot can sell it, so ask
 * for the taker's USDT-to-soft quote; a funded stable means it can buy, so
 * ask the other way. Default to the sell side.
 */
function directionFor(f: Funding): QuoteDirection {
  const softFunded = f.tokens.some((t) => t.role === 'soft' && t.funded === true)
  if (softFunded) return 'usdtToSoft'
  const stableFunded = f.tokens.some((t) => t.role === 'stable' && t.funded === true)
  return stableFunded ? 'softToUsdt' : 'usdtToSoft'
}

/** The headline number: soft per 1 stable (or soft for 1 stable), fee included. */
function headlineFor(p: QuoteProof): string | null {
  if (!p.quote) return null
  const sell = Number(p.quote.sellText)
  const buy = Number(p.quote.buyText)
  if (!(sell > 0) || !(buy > 0)) return null
  if (p.direction === 'usdtToSoft') {
    return live.rateSell(formatRate(buy / sell), p.pair.softSymbol, p.pair.stableSymbol)
  }
  return live.rateBuy(formatRate(sell / buy), p.pair.softSymbol, p.pair.stableSymbol)
}

function ProofBlock({
  proof,
  openMarket,
  error,
  fetching,
  fetchedAgo,
  secondsUntil,
  exhausted,
  unfunded,
  onTryAgain,
  logsHref,
}: {
  proof: QuoteProof | null
  openMarket: QuoteProof | null
  error: ProofError | null
  fetching: boolean
  fetchedAgo: number | null
  secondsUntil: number | null
  exhausted: boolean
  unfunded: boolean
  onTryAgain: () => void
  logsHref: string
}) {
  const retryLine =
    secondsUntil !== null ? (
      <p className="text-xs text-faint">{live.retryingIn(secondsUntil)}</p>
    ) : null
  const tryAgainRow = (
    <div className="flex flex-wrap items-center gap-3">
      <Button variant="secondary" busy={fetching} onClick={onTryAgain}>
        {live.tryAgain}
      </Button>
      <Link className="text-xs underline" to={logsHref}>
        {live.seeLogs}
      </Link>
    </div>
  )

  // A quote in hand: the headline, whoever quoted it.
  if (proof && proof.status === 'preview' && proof.quote) {
    const headline = headlineFor(proof)
    return (
      <div className="space-y-2">
        {fetchedAgo !== null && (
          <p className="text-xs text-faint">{live.fetchedAgo(fetchedAgo)}</p>
        )}
        <p className="text-2xl font-bold tabular-nums">{headline ?? '—'}</p>
        <p className="text-xs text-muted">
          {live.probeNote(formatAmount(proof.quote.sellText), proof.probe.sellSymbol)}
        </p>
        {/* Three states, not two. `null` is the venue not saying who priced
            it, which is not the same as another maker having priced it. */}
        <p
          className={`text-sm ${
            proof.fromThisBot === true
              ? 'text-success'
              : proof.fromThisBot === false
                ? 'text-warning'
                : 'text-muted'
          }`}
        >
          {proof.fromThisBot === true
            ? live.quotedByYou
            : proof.fromThisBot === false
              ? live.quotedByOther
              : live.quotedByUnknown}
        </p>
        <Button variant="ghost" busy={fetching} onClick={onTryAgain}>
          {live.refresh}
        </Button>
      </div>
    )
  }

  // A venue 400: the pair has no RFQ corridor (or the panel says why not).
  if (error && error.status !== 502 && error.status !== 503 && error.status !== 504 && error.status !== 0) {
    const noCorridor = /corridor/i.test(error.message)
    return (
      <div className="space-y-2">
        {noCorridor && <p className="text-base font-bold">{live.noCorridorTitle}</p>}
        <Banner tone="danger">{error.message}</Banner>
        {tryAgainRow}
      </div>
    )
  }

  if (unfunded) {
    return (
      <div className="space-y-2">
        <p className="text-base font-bold">{live.unfundedTitle}</p>
        <p className="text-sm text-muted">{live.unfundedBody}</p>
        {tryAgainRow}
      </div>
    )
  }

  // Nothing asked yet, or asking now.
  if (!proof && !error) {
    return (
      <div className="space-y-2">
        <p className="flex items-center gap-2 text-sm">
          <Spinner /> {live.asking}
        </p>
        <p className="text-xs text-muted">{live.firstPrices}</p>
      </div>
    )
  }

  // The venue was away.
  if (error) {
    return (
      <div className="space-y-2">
        <p className="text-sm">{live.venueDown}</p>
        <p className="text-xs text-faint">{error.message}</p>
        {exhausted ? tryAgainRow : retryLine}
      </div>
    )
  }

  // A no_quote answer. The depth is not always in the probe's sell token (a
  // buy probe is exact-output, so the venue answers in the token it would
  // hand over), and the probe size is not always the sell side either, so both
  // numbers carry their own ticker.
  const depth = proof?.availableText ?? null
  const probeText = proof ? (proof.probe.sellText ?? proof.probe.buyText) : null
  const probeSymbol = proof
    ? proof.probe.sellText !== null
      ? proof.probe.sellSymbol
      : proof.probe.buySymbol
    : null
  const openLine =
    openMarket?.quote && openMarket.direction === 'usdtToSoft'
      ? live.openMarket(
          formatRate(Number(openMarket.quote.buyText) / Number(openMarket.quote.sellText)),
          openMarket.pair.softSymbol,
          openMarket.pair.stableSymbol,
        )
      : null
  return (
    <div className="space-y-2">
      {proof?.reason === 'no_valid_quote' && depth && probeText && probeSymbol ? (
        <p className="text-sm">
          {live.depth(
            formatAmount(probeText),
            probeSymbol,
            formatAmount(depth),
            proof.availableSymbol ?? probeSymbol,
          )}
        </p>
      ) : (
        <p className="flex items-center gap-2 text-sm">
          {!exhausted && <Spinner />}
          {exhausted ? live.stillNone : live.noQuoteYet}
        </p>
      )}
      {!exhausted && <p className="text-xs text-muted">{live.firstPrices}</p>}
      {openLine && <p className="text-xs text-muted">{openLine}</p>}
      {exhausted ? tryAgainRow : retryLine}
    </div>
  )
}

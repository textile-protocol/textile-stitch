// The bot page's Funds tab: where money goes in and comes out.
//
// Deposit is the wallet address plus live balances, the same rows the wizard
// shows. Withdraw is a form that runs `stitch withdraw` as a one-shot, the way
// Tools runs `stitch approve`: the bot's own binary signs with the bot's own
// key, the panel only relays the output. It is refused while the bot (or a
// sibling on the wallet) is running at all: even a maker-only bot quotes
// against the balance a withdraw would drain, and the fills it already
// committed to would fail. The form offers Stop right there rather than
// sending the operator to another tab to find it. The server applies the
// same rule (`canWithdraw` is its answer), so the form only shows it before
// the click.
//
// A vault maker's assets are the vault's: the rows above are read there, which
// is where the balance its quotes draw on actually is. Gas stays the signer
// wallet's, because that is what pays for the transactions.
//
// The Withdraw form stays on a vault maker, titled and framed as the signing
// key's: the vault's money is not reachable from here (it leaves by the vault's
// own redeem flow), but the key's own gas and anything sent to it by mistake
// are, and a withdraw is the only way to either. Hiding the card would make
// them unreachable and Remove would delete the key with them.
//
// This is also where the wizard lands a freshly live bot.

import { useEffect, useRef, useState } from 'react'
import { api } from '../api'
import { botLabel } from '../botRoutes'
import { fundsFromVault } from '../capital'
import { formatAmount, isAddress, shortAddress } from '../format'
import { walletDust } from '../funding'
import { LEVEL_CLASS, appendLine } from '../logBuffer'
import { streamSse } from '../sse'
import { Banner, Button, Card, Field, Input, Select } from './ui'
import {
  GasRow,
  TokenRow,
  VaultAddress,
  orderTokens,
  orderedTokens,
} from './wizard/FundingRows'
import type { Bot, ExitEvent, Funding, FundingToken, LogLine } from '../types'

const NATIVE = 'native'

/** Same bar the binary applies: a plain decimal, no sign, no exponent. */
const isDecimal = (s: string) => /^(\d+\.?\d*|\.\d+)$/.test(s.trim())

export default function FundsTab({
  bot,
  funding,
  busy,
  onStop,
  onStart,
  onWithdrew,
}: {
  bot: Bot
  /** The wallet as the page last read it; the page polls and owns it. */
  funding: Funding | null
  /** Which lifecycle action the page is running, if any. */
  busy: string | null
  /** Stop the named bot: this one, or the sibling that holds the wallet. */
  onStop: (target: string) => void
  onStart: () => void
  /** A withdraw landed: the page re-reads the bot and the wallet. */
  onWithdrew: () => void
}) {
  const address = funding?.operatorAddress ?? null
  const vaulted = fundsFromVault(bot.config)
  // What the signing key itself still holds. Empty on a clean vault maker,
  // and the whole balance sheet on every other bot.
  const dust = walletDust(funding)

  return (
    <div className="space-y-4">
      <Card title="Assets">
        <div className="space-y-3">
          {!funding && <p className="text-sm text-muted">Reading the wallet.</p>}
          {/* No address block here: the header carries the address, its copy
              button and the explorer link, on every tab. */}
          {funding && !address && (
            <Banner tone="warning">
              This bot has no wallet address the panel can read, so there is nothing to
              fund here.
            </Banner>
          )}
          {funding && vaulted && (
            <p className="text-sm text-muted">
              This bot quotes from an OperatorVault, so these are the vault&rsquo;s
              quotable balances, not the signing key&rsquo;s.{' '}
              <VaultAddress funding={funding} />
            </p>
          )}
          {funding && (
            <ul className="divide-y divide-line-soft rounded-lg border border-line-soft">
              {orderedTokens(funding).map((t) => (
                <TokenRow key={t.token} token={t} />
              ))}
              {/* Without a vault this is the same wallet as the rows above, so
                  the gas sits with them. With one it belongs to the signer
                  wallet's own list, below. */}
              {!vaulted && <GasRow funding={funding} pill={false} />}
            </ul>
          )}
          {funding && vaulted && <SignerWallet funding={funding} dust={dust} />}
        </div>
      </Card>

      <Card title={vaulted ? 'Withdraw from the signer wallet' : 'Withdraw'}>
        {vaulted && (
          <p className="mb-4 text-sm text-muted">
            Only what the signing key holds: its gas, and anything sent to it by
            mistake. The vault&rsquo;s money is not reachable from here — it leaves by
            the vault&rsquo;s own redeem flow.
          </p>
        )}
        {funding ? (
          <WithdrawForm
            bot={bot}
            funding={funding}
            tokens={vaulted ? (funding.walletTokens ?? []) : funding.tokens}
            busy={busy}
            onStop={onStop}
            onStart={onStart}
            onWithdrew={onWithdrew}
          />
        ) : (
          <p className="text-sm text-muted">Reading the wallet.</p>
        )}
      </Card>
    </div>
  )
}

/**
 * The signing key's own balance sheet, under the vault's: the gas it pays
 * transactions with, and anything that landed on it that shouldn't have. Kept
 * separate because these are the balances that die with the key — the vault's
 * do not.
 */
function SignerWallet({ funding, dust }: { funding: Funding; dust: FundingToken[] }) {
  return (
    <div>
      <p className="mb-2 text-xs font-bold uppercase tracking-wide text-faint">
        Signer wallet
      </p>
      <ul className="divide-y divide-line-soft rounded-lg border border-line-soft">
        {orderTokens(dust).map((t) => (
          <TokenRow key={t.token} token={t} />
        ))}
        <GasRow funding={funding} pill={false} />
      </ul>
      {dust.length > 0 && (
        <p className="mt-2 text-xs text-warning">
          Corridor tokens on the signing key are not traded — the bot quotes against
          the vault. Withdraw them below.
        </p>
      )}
    </div>
  )
}

type Choice = { key: string; label: string; balanceText: string | null; decimals: number }

/** The tokens the wallet can send: each corridor token, then the gas coin. */
function choicesOf(rows: FundingToken[], funding: Funding): { tokens: Choice[]; native: Choice } {
  return {
    tokens: orderTokens(rows).map((t) => ({
      key: t.token,
      label: t.symbol,
      balanceText: t.balanceText,
      decimals: t.decimals,
    })),
    native: {
      key: NATIVE,
      label: `${funding.gas.symbol} (gas)`,
      balanceText: funding.gas.balanceText,
      decimals: 18,
    },
  }
}

/** A plain decimal, above zero, with no more places than the token has. */
function amountOk(amount: string, decimals: number): boolean {
  return (
    isDecimal(amount) &&
    Number(amount) > 0 &&
    (amount.split('.')[1]?.length ?? 0) <= decimals
  )
}

type Phase = 'form' | 'confirm' | 'running' | 'done' | 'failed'

function WithdrawForm({
  bot,
  funding,
  tokens: rows,
  busy,
  onStop,
  onStart,
  onWithdrew,
}: {
  bot: Bot
  funding: Funding
  /** The balances this key can actually send: the signer wallet's own, which
   * on a vault maker is not what the Assets rows above show. */
  tokens: FundingToken[]
  busy: string | null
  onStop: (target: string) => void
  onStart: () => void
  onWithdrew: () => void
}) {
  const { tokens, native } = choicesOf(rows, funding)
  const choices = [...tokens, native]
  const [tokenKey, setTokenKey] = useState(tokens[0]?.key ?? NATIVE)
  const [amount, setAmount] = useState('')
  const [all, setAll] = useState(false)
  const [to, setTo] = useState('')
  const [phase, setPhase] = useState<Phase>('form')
  const [lines, setLines] = useState<LogLine[]>([])
  const [error, setError] = useState<string | null>(null)
  const [exit, setExit] = useState<ExitEvent | null>(null)
  const active = useRef<{ abort: () => void } | null>(null)

  useEffect(() => () => active.current?.abort(), [])

  const chosen = choices.find((c) => c.key === tokenKey) ?? native
  const amountText = all ? 'all' : amount.trim()
  const own = (funding.operatorAddress ?? '').toLowerCase()
  const toOwnWallet = to.trim().toLowerCase() === own
  const toOk = isAddress(to) && !toOwnWallet
  // The server's answer to "is anything quoting from this wallet right now".
  const blockedBy = bot.canWithdraw ? null : (bot.withdrawBlockedBy ?? bot.name)
  const formOk = (all || amountOk(amount, chosen.decimals)) && toOk && blockedBy === null

  // The bot's own address is never a destination: moving money to itself
  // is a no-op that costs gas, and it is the address most likely to be on
  // the clipboard on this page.
  const toHint = !to.trim()
    ? 'The wallet to send to. Paste the full 0x… address.'
    : !isAddress(to)
      ? 'That is not a full address.'
      : toOwnWallet
        ? 'That is this bot’s own wallet.'
        : `Sends to ${shortAddress(to.trim())} on ${funding.networkLabel ?? `chain ${funding.chainId}`}.`

  const shownAmount = all
    ? `all of the ${chosen.label}${chosen.balanceText ? ` (${formatAmount(chosen.balanceText)})` : ''}`
    : `${amount.trim()} ${chosen.label}`

  function run() {
    active.current?.abort()
    setLines([])
    setExit(null)
    setError(null)
    setPhase('running')
    active.current = streamSse(
      api.withdrawUrl(bot.name),
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ token: tokenKey, amount: amountText, to: to.trim() }),
      },
      {
        onEvent: (event, data) => {
          if (event === 'line') {
            setLines((prev) => appendLine(prev, data as LogLine))
          } else if (event === 'exit') {
            const result = data as ExitEvent
            setExit(result)
            setPhase(result.ok ? 'done' : 'failed')
            if (result.ok) onWithdrew()
          } else if (event === 'error') {
            setError((data as { message: string }).message)
            setPhase('failed')
          }
        },
        // The stream closed without the bot reporting. The one-shot may
        // still be running, or may have sent: say so rather than "failed".
        onDone: () => {
          setPhase((p) => {
            if (p === 'running') {
              setError(
                'The connection dropped before the bot reported. Check the wallet before sending again.',
              )
              return 'failed'
            }
            return p
          })
        },
        onError: (message) => {
          setError(message)
          setPhase('failed')
        },
      },
    )
  }

  function reset() {
    setPhase('form')
    setAmount('')
    setAll(false)
    setTo('')
    setLines([])
    setExit(null)
    setError(null)
  }

  if (phase === 'running' || phase === 'done' || phase === 'failed') {
    return (
      <WithdrawResult
        phase={phase}
        summary={`${shownAmount} to ${shortAddress(to.trim())}`}
        lines={lines}
        exit={exit}
        error={error}
        canStart={phase === 'done' && !bot.running}
        starting={busy === 'start'}
        onReset={reset}
        onStart={onStart}
      />
    )
  }

  if (phase === 'confirm') {
    return (
      <div className="space-y-3">
        <Banner tone="warning">
          <p className="font-bold">Send {shownAmount}?</p>
          <p className="mt-1 break-all font-mono text-sm">{to.trim()}</p>
          <p className="mt-1 text-sm">
            On {funding.networkLabel ?? `chain ${funding.chainId}`}. Signed by the bot’s own
            key. Once sent there is no undo.
          </p>
        </Banner>
        <div className="flex flex-wrap items-center gap-3">
          <Button variant="primary" onClick={run}>
            Yes, send it
          </Button>
          <Button onClick={() => setPhase('form')}>Back</Button>
        </div>
      </div>
    )
  }

  return (
    <div className="space-y-4">
      {blockedBy !== null && (
        <Banner tone="warning">
          <p>
            <strong>Stop {blockedBy === bot.name ? 'the bot' : blockedBy} first.</strong>{' '}
            {bot.withdrawBlockedReason ??
              'It has live quotes against this balance; withdrawing under them fails fills.'}
          </p>
          <div className="mt-2">
            <Button busy={busy === 'stop'} onClick={() => onStop(blockedBy)}>
              Stop {blockedBy === bot.name ? botLabel(bot) : blockedBy}
            </Button>
          </div>
        </Banner>
      )}

      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
        <Field label="Token">
          <Select
            value={tokenKey}
            onChange={(e) => {
              setTokenKey(e.target.value)
              setAll(false)
              setAmount('')
            }}
          >
            {choices.map((c) => (
              <option key={c.key} value={c.key}>
                {c.label}
                {c.balanceText !== null ? ` · ${formatAmount(c.balanceText)}` : ''}
              </option>
            ))}
          </Select>
        </Field>
        <Field
          label="Amount"
          hint={
            all
              ? `Everything in the wallet${tokenKey === NATIVE ? ', minus what the transfer itself costs' : ''}.`
              : amount.trim() && !amountOk(amount, chosen.decimals)
                ? `A plain number with at most ${chosen.decimals} decimals.`
                : `In ${chosen.label}.`
          }
        >
          <div className="flex items-center gap-2">
            <Input
              value={all ? (chosen.balanceText ?? '') : amount}
              disabled={all}
              inputMode="decimal"
              placeholder="0.00"
              onChange={(e) => setAmount(e.target.value)}
            />
            <Button
              variant={all ? 'primary' : 'secondary'}
              onClick={() => setAll((v) => !v)}
              title="Send the whole balance"
            >
              All
            </Button>
          </div>
        </Field>
      </div>

      <Field label="To" hint={toHint}>
        <Input
          value={to}
          placeholder="0x…"
          spellCheck={false}
          onChange={(e) => setTo(e.target.value)}
        />
      </Field>

      <div className="flex flex-wrap items-center gap-3">
        <Button variant="primary" disabled={!formOk} onClick={() => setPhase('confirm')}>
          Withdraw
        </Button>
      </div>
    </div>
  )
}

/** The one-shot's output while it runs, and the outcome once it has. */
function WithdrawResult({
  phase,
  summary,
  lines,
  exit,
  error,
  canStart,
  starting,
  onReset,
  onStart,
}: {
  phase: 'running' | 'done' | 'failed'
  summary: string
  lines: LogLine[]
  exit: ExitEvent | null
  error: string | null
  canStart: boolean
  starting: boolean
  onReset: () => void
  onStart: () => void
}) {
  return (
    <div className="space-y-3">
      <p className="text-sm">
        {phase === 'running' && `Sending ${summary}.`}
        {phase === 'done' && `Sent ${summary}.`}
        {phase === 'failed' && 'The withdraw did not go through.'}
      </p>
      {error && <Banner tone="danger">{error}</Banner>}
      {exit && !exit.ok && !error && (
        <Banner tone="danger">The bot exited with code {exit.code}. The output below says why.</Banner>
      )}
      {lines.length > 0 && (
        <pre className="max-h-64 overflow-auto rounded-lg bg-canvas p-3 font-mono text-xs leading-relaxed">
          {lines.map((l, i) => (
            <div key={i} className={LEVEL_CLASS[l.level]}>
              {l.text}
            </div>
          ))}
        </pre>
      )}
      {phase !== 'running' && (
        <div className="flex flex-wrap items-center gap-3">
          <Button variant="primary" onClick={onReset}>
            {phase === 'done' ? 'Done' : 'Back'}
          </Button>
          {canStart && (
            <Button busy={starting} onClick={onStart}>
              Start the bot again
            </Button>
          )}
        </div>
      )}
    </div>
  )
}

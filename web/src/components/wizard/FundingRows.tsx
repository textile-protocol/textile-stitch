// The pieces the screens that talk about the bot's wallet share: the address
// block (copy, explorer, "this network only"), and one row per thing the
// wallet can hold. The wizard's Approve step shows the gas row and a row per
// approval; the bot page's Funds tab shows the token rows and the gas row as a
// balance sheet. Neither screen owns these, so neither can drift from the
// other's wording.

import { useState } from 'react'
import { formatAmount, formatUsd, groupAddress, hostOf, shortAddress } from '../../format'
import { Button } from '../ui'
import { fund } from './wizardCopy'
import type { Funding, FundingToken } from '../../types'

/** Stable side first, so the order matches the pair's name (`cNGN / USDT`
 * reads soft-first, but the money an operator sends first is the dollar). */
export function orderTokens(tokens: FundingToken[]): FundingToken[] {
  return [...tokens].sort((a, b) =>
    a.role === b.role ? 0 : a.role === 'stable' ? -1 : 1,
  )
}

/** The same order, for the rows the bot quotes against. */
export function orderedTokens(f: Funding): FundingToken[] {
  return orderTokens(f.tokens)
}

/**
 * Where the vault is, in one line, linked when the chain has an explorer.
 *
 * Deliberately not an [`AddressBlock`]: nobody should be told to send tokens
 * to a vault. It issues shares for deposits through its own epochs, and a
 * plain transfer to it mints none — the money would be a donation to the LPs.
 * So the vault is shown to be read, not to be pasted into a wallet.
 */
export function VaultAddress({ funding }: { funding: Funding }) {
  const address = funding.capitalAddress
  if (!address) return null
  const host = hostOf(funding.capitalExplorerUrl)
  return (
    <span className="whitespace-nowrap">
      <span className="font-mono text-xs text-ink" title={address}>
        {shortAddress(address)}
      </span>
      {funding.capitalExplorerUrl && host && (
        <a
          className="ml-2 text-xs text-accent underline"
          href={funding.capitalExplorerUrl}
          target="_blank"
          rel="noreferrer"
        >
          {fund.viewOn(host)}
        </a>
      )}
    </span>
  )
}

/** The wallet to send to, and the one warning that matters when sending. */
export function AddressBlock({ funding, address }: { funding: Funding; address: string }) {
  const [copied, setCopied] = useState(false)
  const [copyError, setCopyError] = useState<string | null>(null)
  const network = funding.networkLabel ?? `chain ${funding.chainId}`
  const explorerHost = hostOf(funding.explorerUrl)

  async function copyAddress() {
    try {
      await navigator.clipboard.writeText(address)
      setCopyError(null)
      setCopied(true)
      window.setTimeout(() => setCopied(false), 2000)
    } catch {
      setCopyError(fund.copyFailed)
    }
  }

  return (
    <div className="rounded-lg border border-line-soft bg-canvas p-4">
      <p className="text-sm font-bold">{fund.addressLabel(network)}</p>
      {/* Groups are separate spans with a margin, not spaces in the text:
          a select-and-copy of the line yields the exact address. */}
      <p className="mt-2 break-all font-mono text-base tabular-nums">
        {groupAddress(address).map((group, i) => (
          <span key={i} className={i > 0 ? 'ml-1.5' : ''}>
            {group}
          </span>
        ))}
      </p>
      <div className="mt-3 flex flex-wrap items-center gap-3">
        <Button variant="primary" onClick={() => void copyAddress()}>
          {copied ? fund.copied : fund.copyAddress}
        </Button>
        {funding.explorerUrl && explorerHost && (
          <a
            className="text-sm text-accent underline"
            href={funding.explorerUrl}
            target="_blank"
            rel="noreferrer"
          >
            {fund.viewOn(explorerHost)}
          </a>
        )}
      </div>
      {copyError && <p className="mt-2 text-xs text-danger">{copyError}</p>}
      <p className="mt-3 text-sm font-bold text-warning">{fund.chainWarning(network)}</p>
    </div>
  )
}

/** One corridor token the wallet holds: symbol, balance, dollars. When the
 * panel could read the balance but not price it, the server's own reason is
 * shown verbatim: it knows whether a feed is down or the pair simply has no
 * dollar price. */
export function TokenRow({ token: t }: { token: FundingToken }) {
  return (
    <li className="grid grid-cols-[1fr_auto] items-center gap-3 px-3 py-2.5">
      <div className="min-w-0">
        <span className="text-sm font-bold text-ink">{t.symbol}</span>
        {t.price === null && t.balance !== null && t.balance !== '0' && (
          <p className="text-xs text-warning">{fund.priceError(t.symbol, t.priceError)}</p>
        )}
      </div>
      <div className="text-right tabular-nums">
        <p className="text-sm">
          {t.balanceText !== null ? `${formatAmount(t.balanceText)} ${t.symbol}` : '—'}
        </p>
        <p className="text-xs text-muted">{formatUsd(t.usd)}</p>
      </div>
    </li>
  )
}

/**
 * A token the bot needs Permit2 approved for. Approval is permission, not
 * money: it lets Textile's settlement pull this token from the wallet against
 * an order the bot signed, and it takes one transaction paid from gas. So the
 * row says "Approved" or "Needs approval", never a balance.
 */
export function ApprovalRow({ token: t }: { token: FundingToken }) {
  const pill: Pill =
    t.approved === true
      ? { tone: 'success', label: fund.pill.approved, done: true }
      : t.approved === null
        ? { tone: 'warning', label: fund.pill.unknown }
        : { tone: 'muted', label: fund.pill.needsApproval }
  return (
    <li className="grid grid-cols-[auto_1fr_auto] items-center gap-3 px-3 py-2.5">
      <StatusPill pill={pill} />
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-sm font-bold text-ink">{t.symbol}</span>
          <span className="font-mono text-xs text-faint" title={t.token}>
            {shortAddress(t.token)}
          </span>
        </div>
        <p className="text-xs text-faint">
          {t.approved === true ? fund.approvedHint : fund.approvalHint(t.symbol)}
        </p>
      </div>
      <div />
    </li>
  )
}

/**
 * The gas row carries no hint of its own: the card title already says how
 * much to deposit, and the gate line under the rows says when it is enough.
 * Saying it a third time here read as three different instructions.
 */
export function GasRow({ funding, pill: showPill = true }: { funding: Funding; pill?: boolean }) {
  const g = funding.gas
  const pill: Pill =
    g.balance === null || g.ok === null
      ? { tone: 'warning', label: fund.pill.unknown }
      : g.ok
        ? { tone: 'success', label: fund.pill.ok, done: true }
        : { tone: 'muted', label: fund.pill.addGas }
  return (
    <li
      className={`grid items-center gap-3 px-3 py-2.5 ${
        showPill ? 'grid-cols-[auto_1fr_auto]' : 'grid-cols-[1fr_auto]'
      }`}
    >
      {showPill && <StatusPill pill={pill} />}
      <div className="min-w-0">
        <p className="text-sm font-bold text-ink">
          {g.symbol} <span className="font-normal text-faint">for gas</span>
        </p>
      </div>
      <div className="text-right tabular-nums">
        <p className="text-sm">
          {g.balanceText !== null ? `${formatAmount(g.balanceText)} ${g.symbol}` : '—'}
        </p>
        <p className="text-xs text-muted">
          {formatUsd(g.usd)}
          {g.priceSource === 'fallback' && g.usd !== null ? ` ${fund.estimated}` : ''}
        </p>
      </div>
    </li>
  )
}

interface Pill {
  tone: 'success' | 'warning' | 'muted'
  label: string
  done?: boolean
}

function StatusPill({ pill }: { pill: Pill }) {
  const tone = {
    success: 'bg-success-bg text-success',
    warning: 'bg-warning-bg text-warning',
    muted: 'bg-hover text-muted',
  }[pill.tone]
  return (
    <span
      className={`inline-flex min-w-24 shrink-0 items-center justify-center gap-1 whitespace-nowrap rounded-full px-2.5 py-0.5 text-xs font-bold ${tone}`}
    >
      {pill.done && <span aria-hidden>✓</span>}
      {pill.label}
    </span>
  )
}

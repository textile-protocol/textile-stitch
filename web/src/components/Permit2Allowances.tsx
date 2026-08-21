import { useCallback, useEffect, useState } from 'react'
import { ApiError, api } from '../api'
import { Banner, Button, Spinner, Tag } from './ui'
import type { Allowances } from '../types'

/**
 * Permit2 allowance status for every token this bot's corridors spend.
 *
 * One approval covers a token across every pool that pays it, so this lists
 * tokens rather than corridors — a bot quoting cNGN/USDT and wBRL/USDT needs
 * three approvals, not four, and the shared USDT row says which pairs rely on
 * it. Before this the panel could only say "some approval is missing", which is
 * unhelpful the moment a bot has more than one pair.
 */
export default function Permit2Allowances({
  bot,
  refreshKey = 0,
}: {
  bot: string
  /** Bump to re-read after an approval run. */
  refreshKey?: number
}) {
  const [data, setData] = useState<Allowances | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)

  const load = useCallback(
    async (signal?: { cancelled: boolean }) => {
      setLoading(true)
      try {
        const res = await api.allowances(bot)
        if (signal?.cancelled) return
        setData(res)
        setError(null)
      } catch (e) {
        if (signal?.cancelled) return
        setError(e instanceof ApiError ? e.message : String(e))
      } finally {
        if (!signal?.cancelled) setLoading(false)
      }
    },
    [bot],
  )

  useEffect(() => {
    const signal = { cancelled: false }
    void load(signal)
    return () => {
      signal.cancelled = true
    }
  }, [load, refreshKey])

  if (loading && !data) {
    return (
      <div className="flex items-center gap-2 py-4 text-sm text-muted">
        <Spinner /> Checking allowances on chain…
      </div>
    )
  }
  if (error && !data) return <Banner tone="danger">{error}</Banner>
  if (!data) return null

  const missing = data.tokens.filter((t) => t.approved === false)
  const unknown = data.tokens.filter((t) => t.approved === null)

  return (
    <div className="space-y-3">
      {error ? (
        <Banner tone="danger">
          Latest check failed, so the rows below may be stale: {error}
        </Banner>
      ) : data.readError ? (
        <Banner tone="warning">
          Couldn&apos;t read allowances from the chain, so the rows below say
          unknown rather than guessing: {data.readError}
        </Banner>
      ) : missing.length > 0 ? (
        <Banner tone="warning">
          {missing.length === 1
            ? `${missing[0]!.symbol} is not approved yet. `
            : `${missing.length} tokens are not approved yet. `}
          Until they are, this bot can post orders that fail to fill. Run{' '}
          <strong>Approve allowances</strong> under One-off runs.
        </Banner>
      ) : (
        <Banner tone="success">
          Every token these corridors spend is approved to Permit2.
        </Banner>
      )}

      <ul className="divide-y divide-line-soft rounded-lg border border-line-soft">
        {data.tokens.map((t) => (
          <li
            key={t.token}
            className="flex flex-wrap items-center justify-between gap-3 px-3 py-2.5"
          >
            <div className="min-w-0">
              <div className="flex items-center gap-2">
                <span className="text-sm font-bold text-ink">{t.symbol}</span>
                <span className="font-mono text-xs text-faint">{t.token}</span>
              </div>
              <div className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1.5">
                <span className="flex flex-wrap items-center gap-1.5">
                  {t.corridors.map((c) => (
                    <Tag key={c}>{c}</Tag>
                  ))}
                </span>
                <span className="text-xs text-muted">
                  {t.usesMaxLiquidity
                    ? 'commits the whole balance'
                    : `commits ${t.required}`}
                </span>
              </div>
            </div>
            <AllowanceStatus approved={t.approved} />
          </li>
        ))}
      </ul>

      {unknown.length > 0 && !data.readError && (
        <p className="text-xs text-faint">
          A token reads as unknown when the panel could not call it on this
          chain. Check the RPC URL on the Settings tab.
        </p>
      )}

      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-xs text-faint">
          Read from the chain. Re-check after approving from anywhere else.
        </p>
        <Button variant="ghost" busy={loading} onClick={() => void load()}>
          Re-check
        </Button>
      </div>

      <p className="text-xs text-faint">
        Approving grants Permit2 permission to move that token from the operator
        wallet{' '}
        {data.operatorAddress && (
          <span className="font-mono">{data.operatorAddress}</span>
        )}
        . One approval per token covers every corridor that spends it.
      </p>
    </div>
  )
}

function AllowanceStatus({ approved }: { approved: boolean | null }) {
  const [tone, label] =
    approved === true
      ? ['bg-success-bg text-success', 'Approved']
      : approved === false
        ? ['bg-warning-bg text-warning', 'Not approved']
        : ['bg-hover text-muted', 'Unknown']
  return (
    <span
      className={`inline-flex shrink-0 items-center rounded-full px-2.5 py-0.5 text-xs font-bold ${tone}`}
    >
      {label}
    </span>
  )
}

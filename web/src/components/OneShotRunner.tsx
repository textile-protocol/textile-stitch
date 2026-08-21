import { useEffect, useRef, useState } from 'react'
import { appendLine } from '../logBuffer'
import { streamSse } from '../sse'
import { Banner, Button } from './ui'
import type { ExitEvent, LogLine, LogLevel } from '../types'

const LEVEL_CLASS: Record<LogLevel, string> = {
  error: 'text-danger',
  warn: 'text-warning',
  info: 'text-ink',
  debug: 'text-muted',
  trace: 'text-faint',
  plain: 'text-muted',
}

/**
 * Runs `stitch approve` or a dry run in a throwaway container and streams the
 * output.
 *
 * Approve sends transactions and costs gas, so it asks first. A dry run signs
 * nothing, so it doesn't.
 *
 * Approve is also refused outright while the bot's own process could broadcast
 * from the same wallet — two processes independently reading the pending nonce can
 * sign the same one, and one of the two transactions is then lost. The API enforces
 * that; `approveBlockedReason` is so the operator reads it instead of clicking.
 */
export default function OneShotRunner({
  bot,
  canApprove,
  approveBlockedReason,
  approveBlockedBy,
  highlightPermit2 = false,
  onApproved,
  onStopForApproval,
}: {
  bot: string
  canApprove: boolean
  approveBlockedReason: string | null
  /** Live bot spending this wallet's nonce. Stop that one, not necessarily `bot`. */
  approveBlockedBy: string | null
  /** After create: surface the Permit2 + gas requirement up front. */
  highlightPermit2?: boolean
  /** Fired when `stitch approve` exits successfully. */
  onApproved?: () => void
  /**
   * Stop the bot named by `approveBlockedBy` so an approval can run. Approve is
   * refused while any process on this wallet could broadcast, which is a dead
   * end for a bot crash-looping *because* a token is unapproved. Stopping is the
   * way out — and it has to be the bot the backend named, not always this page's
   * bot. A sibling on the same wallet blocks approval just as hard.
   */
  onStopForApproval?: (target: string) => void
}) {
  const [lines, setLines] = useState<LogLine[]>([])
  const [running, setRunning] = useState<string | null>(null)
  const [exit, setExit] = useState<ExitEvent | null>(null)
  const [error, setError] = useState<string | null>(null)
  const active = useRef<{ abort: () => void } | null>(null)

  // Navigating away has to close the stream. The server reaps the throwaway
  // container when the connection drops, so an abandoned fetch means a dry run —
  // which loops until it's told to stop — keeps polling forever, one container
  // per navigation.
  useEffect(() => () => active.current?.abort(), [])

  function run(action: 'approve' | 'dry-run') {
    if (
      action === 'approve' &&
      !window.confirm(
        `Approve Permit2 for ${bot}'s input tokens?\n\nThis sends transactions from its operator wallet and costs a little gas. Without this approval the bot cannot trade.`,
      )
    ) {
      return
    }

    active.current?.abort()
    setLines([])
    setExit(null)
    setError(null)
    setRunning(action)

    active.current = streamSse(
      `/api/bots/${encodeURIComponent(bot)}/${action}`,
      { method: 'POST' },
      {
        onEvent: (event, data) => {
          if (event === 'line') {
            setLines((prev) => appendLine(prev, data as LogLine))
          } else if (event === 'exit') {
            const result = data as ExitEvent
            setExit(result)
            setRunning(null)
            if (result.ok && result.action === 'approve') {
              onApproved?.()
            }
          } else if (event === 'error') {
            setError((data as { message: string }).message)
            setRunning(null)
          }
        },
        onDone: () => setRunning(null),
        onError: (message) => {
          setError(message)
          setRunning(null)
        },
      },
    )
  }

  return (
    <div className="space-y-3">
      {highlightPermit2 && (
        <Banner tone="warning">
          Before the first live start, run <strong>Approve allowances</strong>. That
          grants Permit2 permission on each input token. The operator wallet needs a
          little native gas for those one-time approval transactions — without them,
          orders would post but fail to fill.
        </Banner>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <Button
          busy={running === 'dry-run'}
          disabled={running !== null}
          onClick={() => run('dry-run')}
          title="Load the config and price a tick without signing or posting anything"
        >
          Dry run
        </Button>
        <Button
          busy={running === 'approve'}
          disabled={running !== null || !canApprove}
          onClick={() => run('approve')}
          title={
            approveBlockedReason ??
            'Approve input tokens to Permit2. Sends transactions and costs gas.'
          }
        >
          Approve allowances
        </Button>
        {running && (
          <Button
            variant="ghost"
            onClick={() => {
              active.current?.abort()
              setRunning(null)
            }}
          >
            Stop watching
          </Button>
        )}
      </div>

      <p className="text-xs text-faint">
        Both run in a throwaway container with this bot&apos;s own config and key. A
        dry run posts nothing; Approve allowances sends Permit2 approval
        transactions and costs a little gas.
      </p>

      {!canApprove && approveBlockedReason && (
        <Banner tone="warning">
          <div className="space-y-2">
            <p>{approveBlockedReason}</p>
            {onStopForApproval && approveBlockedBy && (
              <Button
                variant="secondary"
                onClick={() => onStopForApproval(approveBlockedBy)}
              >
                {approveBlockedBy === bot
                  ? `Stop ${bot} so it can be approved`
                  : `Stop ${approveBlockedBy} so ${bot} can be approved`}
              </Button>
            )}
          </div>
        </Banner>
      )}

      {error && <Banner tone="danger">{error}</Banner>}

      {exit && (
        <Banner tone={exit.ok ? 'success' : 'danger'}>
          {exit.ok
            ? `${exit.action} finished cleanly.`
            : `${exit.action} exited with code ${exit.code}. The output above says why.`}
        </Banner>
      )}

      {lines.length > 0 && (
        <div className="max-h-72 overflow-auto rounded-lg bg-canvas p-3 font-mono text-xs leading-relaxed">
          {lines.map((line, i) => (
            <div key={i} className={`whitespace-pre-wrap ${LEVEL_CLASS[line.level]}`}>
              {line.text}
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

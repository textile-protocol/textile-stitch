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
}: {
  bot: string
  canApprove: boolean
  approveBlockedReason: string | null
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
        `Approve the router allowances for ${bot}? This sends transactions from its operator wallet and costs gas.`,
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
            setExit(data as ExitEvent)
            setRunning(null)
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
            'Grant the allowances the bot needs to trade. Sends transactions.'
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
        Both run in a throwaway container with this bot's own config and key. A dry
        run posts nothing; approve sends transactions and costs gas.
      </p>

      {!canApprove && approveBlockedReason && (
        <Banner tone="warning">{approveBlockedReason}</Banner>
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

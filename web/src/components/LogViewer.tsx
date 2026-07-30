import { useEffect, useRef, useState } from 'react'
import { api } from '../api'
import { appendLine, MAX_LINES } from '../logBuffer'
import { streamSse } from '../sse'
import { Button, Banner } from './ui'
import type { LogLine, LogLevel } from '../types'

/** How many historical lines to replay when the tail opens. */
const REPLAY = 500

const LEVEL_CLASS: Record<LogLevel, string> = {
  error: 'text-danger',
  warn: 'text-warning',
  info: 'text-ink',
  debug: 'text-muted',
  trace: 'text-faint',
  plain: 'text-muted',
}

export default function LogViewer({ bot }: { bot: string }) {
  const [lines, setLines] = useState<LogLine[]>([])
  const [error, setError] = useState<string | null>(null)
  const [paused, setPaused] = useState(false)
  const [follow, setFollow] = useState(true)
  const bottom = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (paused) return
    setError(null)
    const stream = streamSse(
      api.logsUrl(bot, REPLAY),
      { method: 'GET' },
      {
        onEvent: (event, data) => {
          if (event === 'line') {
            setLines((prev) => appendLine(prev, data as LogLine))
          } else if (event === 'error') {
            setError((data as { message: string }).message)
          }
        },
        onError: setError,
      },
    )
    return () => stream.abort()
  }, [bot, paused])

  useEffect(() => {
    if (follow) bottom.current?.scrollIntoView({ block: 'end' })
  }, [lines, follow])

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <Button onClick={() => setPaused(!paused)}>
          {paused ? 'Resume' : 'Pause'}
        </Button>
        <Button onClick={() => setLines([])}>Clear</Button>
        <Button
          onClick={() =>
            void navigator.clipboard.writeText(lines.map((l) => l.text).join('\n'))
          }
          title="Copy everything currently buffered"
        >
          Copy
        </Button>
        <label className="ml-auto flex items-center gap-2 text-sm text-muted">
          <input
            type="checkbox"
            checked={follow}
            onChange={(e) => setFollow(e.target.checked)}
            className="size-4 accent-[var(--tx-accent)]"
          />
          Follow
        </label>
      </div>

      {error && <Banner tone="danger">{error}</Banner>}

      {lines.length === 0 && !error ? (
        <p className="py-8 text-center text-sm text-muted">
          {paused ? 'Paused.' : 'Waiting for output…'}
        </p>
      ) : (
        <div className="max-h-96 overflow-auto rounded-lg bg-canvas p-3 font-mono text-xs leading-relaxed">
          {lines.map((line, i) => (
            <div key={i} className={`whitespace-pre-wrap ${LEVEL_CLASS[line.level]}`}>
              {line.text}
            </div>
          ))}
          <div ref={bottom} />
        </div>
      )}

      {lines.length >= MAX_LINES && (
        <p className="text-xs text-faint">
          Showing the last {MAX_LINES} lines. Older output is still in the
          container's log — <code>docker logs</code> has all of it.
        </p>
      )}
    </div>
  )
}

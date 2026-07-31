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
  // Which bot the buffer currently holds, so a switch to another bot clears and
  // replays instead of appending onto the previous bot's lines.
  const streamedBot = useRef<string | null>(null)
  // Whether the last effect run left us paused. A resume — the only case that should
  // skip the replay — is this flipping back off for the same bot.
  const wasPaused = useRef(false)

  useEffect(() => {
    if (paused) {
      wasPaused.current = true
      return
    }
    setError(null)
    const differentBot = streamedBot.current !== bot
    // Replay the tail on every attach except a genuine resume: pausing keeps the lines
    // on screen, so resuming the same bot must not replay the last REPLAY of them again
    // (it would duplicate the overlap and evict older lines from the bounded buffer). A
    // resume is specifically the pause toggle flipping back off for the same bot —
    // *not* a bot switch, and not React StrictMode's dev remount, which re-runs this
    // effect with paused still false and must still replay or the viewer opens empty.
    const resuming = wasPaused.current && !differentBot
    wasPaused.current = false
    if (differentBot) setLines([])
    streamedBot.current = bot
    const stream = streamSse(
      api.logsUrl(bot, resuming ? 0 : REPLAY),
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

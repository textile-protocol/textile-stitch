import { useEffect, useRef, useState } from 'react'
import { api } from '../api'
import { appendLine, LEVEL_CLASS, MAX_LINES } from '../logBuffer'
import { followLog, isDivider, type LogEntry } from '../logFollow'
import { streamSse } from '../sse'
import { Button, Banner } from './ui'

/** How many historical lines to replay when the tail opens. */
const REPLAY = 500

export default function LogViewer({
  bot,
  live,
  attachKey = 0,
}: {
  bot: string
  /**
   * The bot has a process running or about to run (`Bot.canStop`). A stream
   * that ends is re-attached only while this holds: a stopped container's log
   * ends straight away, and there's nothing to follow until it starts again.
   */
  live: boolean
  /**
   * Changed by the page each time it starts, stops, restarts or replaces the
   * container, so the tail re-attaches right away instead of waiting for its
   * next retry. The desktop runtime needs this one: its stream follows a log
   * file, and a recreate swaps the file out without ending the stream.
   */
  attachKey?: number
}) {
  const [lines, setLines] = useState<LogEntry[]>([])
  // Everything received for this bot, bounded like the screen but untouched by
  // Clear. A re-attach lines its replay up against this rather than the screen,
  // so clearing and then resuming doesn't bring the cleared lines back.
  const seen = useRef<LogEntry[]>([])
  const [error, setError] = useState<string | null>(null)
  // The stream ended and the tail is trying to get back onto the bot.
  const [reconnecting, setReconnecting] = useState(false)
  const [paused, setPaused] = useState(false)
  const [follow, setFollow] = useState(true)
  const bottom = useRef<HTMLDivElement>(null)
  // Which bot the buffer currently holds, so a switch to another bot clears and
  // replays instead of appending onto the previous bot's lines.
  const streamedBot = useRef<string | null>(null)

  useEffect(() => {
    if (paused) {
      setReconnecting(false)
      return
    }
    setError(null)
    // Every attach replays the tail: the first one, a resume, a re-attach after a
    // restart. For the same bot `followLog` skips the lines already on screen, so a
    // resume fills in what was printed while paused without repeating the overlap.
    if (streamedBot.current !== bot) {
      seen.current = []
      setLines([])
      setReconnecting(false)
    }
    streamedBot.current = bot
    return followLog({
      open: (tail, handlers) =>
        streamSse(api.logsUrl(bot, tail), { method: 'GET' }, handlers),
      replay: REPLAY,
      live,
      current: () => seen.current,
      onAppend: (entries) => {
        seen.current = appendAll(seen.current, entries)
        setLines((prev) => appendAll(prev, entries))
      },
      onError: setError,
      onReconnecting: setReconnecting,
    })
  }, [bot, paused, live, attachKey])

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

      {/*
        While it's reconnecting, an error is one failed attempt (mid-swap there is
        briefly no container to attach to), not news. Say what's going on instead.
      */}
      {reconnecting ? (
        <p className="text-sm text-muted" role="status">
          The log stream ended. Reconnecting…
          {error && <span className="text-faint"> ({error})</span>}
        </p>
      ) : (
        error && <Banner tone="danger">{error}</Banner>
      )}

      {lines.length === 0 && (!error || reconnecting) ? (
        <p className="py-8 text-center text-sm text-muted">
          {paused ? 'Paused.' : 'Waiting for output…'}
        </p>
      ) : (
        <div className="max-h-96 overflow-auto rounded-lg bg-canvas p-3 font-mono text-xs leading-relaxed">
          {lines.map((entry, i) => (
            <div
              key={i}
              className={
                isDivider(entry)
                  ? 'my-1 text-center text-faint'
                  : `whitespace-pre-wrap ${LEVEL_CLASS[entry.level]}`
              }
            >
              {entry.text}
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

function appendAll(lines: LogEntry[], entries: LogEntry[]): LogEntry[] {
  return entries.reduce((acc, entry) => appendLine(acc, entry), lines)
}

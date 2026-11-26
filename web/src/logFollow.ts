// Following a bot's log across restarts and container swaps.
//
// The panel's log stream follows one container, and it ends when that container
// stops: a Restart, an Update or rollback that recreates it, a crash under the
// restart policy. Left alone, the tail goes quiet and stays quiet while the bot
// carries on logging. So when the stream ends and the bot is still meant to be
// running, `followLog` opens a new one, backing off while there is nothing to
// attach to yet (mid-swap, or a container waiting out its restart delay).
//
// Each new stream replays the last lines of the log, which is the only way to
// pick up what the bot printed at startup before the tail got back to it. Two
// cases follow from Docker:
//
// - Same container (Restart, Start after Stop): Docker keeps one log across
//   runs, so the replay opens with lines that are already on screen.
// - New container (Update, Recreate, rollback): its log starts empty, so the
//   whole replay is new.
//
// Resume after a pause is the same-container case too: the replay opens with
// lines on screen, and only what the bot printed while paused is new.
//
// The stream doesn't say which it is, so the lines decide. A replay that starts
// with a run of lines ending exactly where the screen ends is a repeat, and that
// run is skipped. Anything else is appended after a divider, so the break
// between the old output and the new one shows. Bot lines carry their own
// timestamps, so a false repeat needs two identical runs of timestamped lines.
// When the guess is wrong it shows a line twice and never drops one: lines held
// back as a possible repeat are appended once the replay stops matching.

import type { SseHandlers, SseStream } from './sse'
import type { LogLine } from './types'

/** A marker between two stretches of output that don't continue each other. */
export interface LogDivider {
  divider: true
  text: string
}

/** What the log buffer holds: the bot's lines, plus dividers between streams. */
export type LogEntry = LogLine | LogDivider

export function isDivider(entry: LogEntry): entry is LogDivider {
  return 'divider' in entry
}

export const RECONNECTED: LogDivider = { divider: true, text: '── reconnected ──' }

/** How a new stream lines up with what's already on screen. */
export type Splice =
  /** Every line is new. */
  | { phase: 'append' }
  /** Waiting for the first line, which decides whether the replay repeats the screen. */
  | { phase: 'first'; onScreen: readonly LogLine[] }
  /** The stream is repeating `onScreen[from..next)` and should send `onScreen[next]` next. */
  | { phase: 'repeat'; onScreen: readonly LogLine[]; from: number; next: number }

export const APPEND: Splice = { phase: 'append' }

/** Line up a new stream against `entries`, the buffer as it is now. */
export function startSplice(entries: readonly LogEntry[]): Splice {
  const onScreen = entries.filter((entry): entry is LogLine => !isDivider(entry))
  return onScreen.length === 0 ? APPEND : { phase: 'first', onScreen }
}

export interface SpliceStep {
  state: Splice
  /** What to add to the buffer, in order. Empty while a repeat is skipped. */
  append: LogEntry[]
}

/** Feed one line of the new stream. */
export function spliceLine(state: Splice, line: LogLine): SpliceStep {
  switch (state.phase) {
    case 'append':
      return { state, append: [line] }
    case 'first': {
      const at = lastIndexOfLine(state.onScreen, line)
      return at < 0
        ? { state: APPEND, append: [RECONNECTED, line] }
        : repeating(state.onScreen, at, at + 1)
    }
    case 'repeat':
      return sameLine(state.onScreen[state.next], line)
        ? repeating(state.onScreen, state.from, state.next + 1)
        : {
            state: APPEND,
            append: [RECONNECTED, ...state.onScreen.slice(state.from, state.next), line],
          }
  }
}

/** The stream has repeated `onScreen[from..next)`. At the end of the screen it has caught up. */
function repeating(onScreen: readonly LogLine[], from: number, next: number): SpliceStep {
  return next === onScreen.length
    ? { state: APPEND, append: [] }
    : { state: { phase: 'repeat', onScreen, from, next }, append: [] }
}

function sameLine(a: LogLine | undefined, b: LogLine): boolean {
  return a !== undefined && a.text === b.text && a.stream === b.stream
}

function lastIndexOfLine(lines: readonly LogLine[], line: LogLine): number {
  for (let i = lines.length - 1; i >= 0; i--) {
    if (sameLine(lines[i], line)) return i
  }
  return -1
}

/** Shortest and longest wait before re-attaching an ended stream. */
export const REATTACH_MIN_MS = 1000
export const REATTACH_MAX_MS = 10_000

/**
 * A stream that stays open this long after the panel answers is attached to a
 * running container, even if the bot has printed nothing new yet. A stopped
 * container's stream ends within milliseconds.
 */
export const SETTLED_MS = 2000

/** Wait before the next attach, after `quiet` attaches in a row ended with nothing new. */
export function reattachDelay(quiet: number): number {
  return Math.min(REATTACH_MIN_MS * 2 ** quiet, REATTACH_MAX_MS)
}

export interface FollowOptions {
  /** Open the log stream, replaying the last `tail` lines first. */
  open: (tail: number, handlers: SseHandlers) => SseStream
  /** Lines to replay on each attach. */
  replay: number
  /** Whether to re-attach once a stream ends. Off for a stopped bot: its log ends at once. */
  live: boolean
  /** The buffer as it is now, for lining a replay up against. */
  current: () => readonly LogEntry[]
  /** Entries to add to the buffer, in order. */
  onAppend: (entries: LogEntry[]) => void
  /** The stream failed, or it attached cleanly again (`null`). */
  onError: (message: string | null) => void
  /** The stream ended and a re-attach is pending or under way. */
  onReconnecting: (reconnecting: boolean) => void
}

/** Follow the log until the returned function is called. */
export function followLog(options: FollowOptions): () => void {
  let stopped = false
  let stream: SseStream | null = null
  let retry: ReturnType<typeof setTimeout> | undefined
  let settle: ReturnType<typeof setTimeout> | undefined
  // Attaches in a row that ended without a new line, for the backoff.
  let quiet = 0
  // The current stream has proved itself, by a new line or by staying open.
  let following = false

  const attached = () => {
    if (following) return
    following = true
    quiet = 0
    options.onError(null)
    options.onReconnecting(false)
  }

  const ended = () => {
    clearTimeout(settle)
    following = false
    if (stopped) return
    if (!options.live) {
      options.onReconnecting(false)
      return
    }
    options.onReconnecting(true)
    retry = setTimeout(attach, reattachDelay(quiet))
    quiet += 1
  }

  const attach = () => {
    let splice = startSplice(options.current())
    stream = options.open(options.replay, {
      onOpen: () => {
        settle = setTimeout(attached, SETTLED_MS)
      },
      onEvent: (event, data) => {
        if (event === 'line') {
          const step = spliceLine(splice, data as LogLine)
          splice = step.state
          if (step.append.length === 0) return
          attached()
          options.onAppend(step.append)
        } else if (event === 'error') {
          options.onError((data as { message: string }).message)
        }
      },
      onDone: ended,
      onError: (message) => {
        options.onError(message)
        ended()
      },
    })
  }

  attach()

  return () => {
    stopped = true
    stream?.abort()
    clearTimeout(retry)
    clearTimeout(settle)
  }
}

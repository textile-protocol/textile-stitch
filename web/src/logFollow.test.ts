import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import {
  APPEND,
  RECONNECTED,
  REATTACH_MAX_MS,
  SETTLED_MS,
  followLog,
  reattachDelay,
  spliceLine,
  startSplice,
  type LogEntry,
  type Splice,
} from './logFollow'
import type { SseHandlers } from './sse'
import type { LogLine } from './types'

const line = (text: string, stream: LogLine['stream'] = 'stdout'): LogLine => ({
  text,
  stream,
  level: 'info',
})

const lines = (...texts: string[]) => texts.map((t) => line(t))

/** Run a whole stream through a splice and return what it adds to the buffer. */
function splice(onScreen: LogEntry[], incoming: LogLine[]): LogEntry[] {
  let state: Splice = startSplice(onScreen)
  const added: LogEntry[] = []
  for (const l of incoming) {
    const step = spliceLine(state, l)
    state = step.state
    added.push(...step.append)
  }
  return added
}

describe('splicing a replay onto the screen', () => {
  it('appends everything when the screen is empty, with no divider', () => {
    expect(splice([], lines('a', 'b'))).toEqual(lines('a', 'b'))
    expect(startSplice([])).toBe(APPEND)
  })

  it('skips the lines a same-container replay repeats and keeps the rest', () => {
    // Restart: Docker's log runs on, so the replay opens with what's on screen.
    const screen = lines('t1 boot', 't2 quote', 't3 shutting down')
    const replay = lines('t2 quote', 't3 shutting down', 't4 boot', 't5 quote')
    expect(splice(screen, replay)).toEqual(lines('t4 boot', 't5 quote'))
  })

  it('adds nothing when the replay only repeats the screen', () => {
    const screen = lines('t1', 't2', 't3')
    expect(splice(screen, lines('t1', 't2', 't3'))).toEqual([])
    expect(splice(screen, lines('t3'))).toEqual([])
  })

  it('puts a new container after a divider', () => {
    // Update: the recreated container's log starts empty, so nothing lines up.
    const screen = lines('t1 v1 quote', 't2 v1 shutting down')
    const replay = lines('t3 v2 boot', 't4 v2 quote')
    expect(splice(screen, replay)).toEqual([RECONNECTED, ...lines('t3 v2 boot', 't4 v2 quote')])
  })

  it('shows held lines again when the replay stops matching before the end of the screen', () => {
    // `t2` happens to be on screen, but the replay goes elsewhere after it: it
    // doesn't continue the screen, so nothing it sent may be dropped.
    const screen = lines('t1', 't2', 't3')
    const replay = lines('t2', 'x', 'y')
    expect(splice(screen, replay)).toEqual([RECONNECTED, ...lines('t2', 'x', 'y')])
  })

  it('matches on stream as well as text', () => {
    const screen = [line('same', 'stdout')]
    expect(splice(screen, [line('same', 'stderr')])).toEqual([RECONNECTED, line('same', 'stderr')])
  })

  it('ignores dividers already on screen when lining up', () => {
    const screen: LogEntry[] = [...lines('t1'), RECONNECTED, ...lines('t2', 't3')]
    expect(splice(screen, lines('t2', 't3', 't4'))).toEqual(lines('t4'))
  })

  it('lines up against the latest copy of a repeated line', () => {
    const screen = lines('tick', 'a', 'tick')
    expect(splice(screen, lines('tick', 'b'))).toEqual(lines('b'))
  })
})

describe('reattachDelay', () => {
  it('doubles from a second and stops at the cap', () => {
    expect([0, 1, 2, 3, 4, 10].map(reattachDelay)).toEqual([
      1000,
      2000,
      4000,
      8000,
      REATTACH_MAX_MS,
      REATTACH_MAX_MS,
    ])
  })
})

/** A panel log endpoint the test drives by hand. */
function fakePanel() {
  const opened: { tail: number; handlers: SseHandlers; aborted: boolean }[] = []
  const open = (tail: number, handlers: SseHandlers) => {
    const stream = { tail, handlers, aborted: false }
    opened.push(stream)
    return { abort: () => (stream.aborted = true) }
  }
  const latest = () => {
    const stream = opened[opened.length - 1]
    if (!stream) throw new Error('nothing opened')
    return stream.handlers
  }
  return {
    open,
    opened,
    /** Answer the latest request and send `texts` down it. */
    send: (...texts: string[]) => {
      const handlers = latest()
      handlers.onOpen?.()
      texts.forEach((t) => handlers.onEvent('line', line(t)))
    },
    end: () => latest().onDone?.(),
    fail: (message: string) => latest().onError?.(message),
  }
}

function follow(panel: ReturnType<typeof fakePanel>, live = true) {
  let buffer: LogEntry[] = []
  const errors: (string | null)[] = []
  const reconnecting: boolean[] = []
  const stop = followLog({
    open: panel.open,
    replay: 500,
    live,
    current: () => buffer,
    onAppend: (entries) => {
      buffer = [...buffer, ...entries]
    },
    onError: (message) => errors.push(message),
    onReconnecting: (r) => reconnecting.push(r),
  })
  return {
    stop,
    buffer: () => buffer,
    texts: () => buffer.map((e) => e.text),
    errors,
    reconnecting,
  }
}

describe('followLog', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  it('replays on the first attach and shows every line', () => {
    const panel = fakePanel()
    const view = follow(panel)
    panel.send('t1', 't2')
    expect(panel.opened.map((s) => s.tail)).toEqual([500])
    expect(view.texts()).toEqual(['t1', 't2'])
  })

  it('re-attaches after an Update and appends the new container after a divider', () => {
    const panel = fakePanel()
    const view = follow(panel)
    panel.send('t1 v1', 't2 v1 shutting down')
    panel.end()
    expect(view.reconnecting.at(-1)).toBe(true)
    expect(panel.opened).toHaveLength(1)

    vi.advanceTimersByTime(1000)
    expect(panel.opened).toHaveLength(2)
    panel.send('t3 v2 boot')
    expect(view.texts()).toEqual(['t1 v1', 't2 v1 shutting down', RECONNECTED.text, 't3 v2 boot'])
    expect(view.reconnecting.at(-1)).toBe(false)
  })

  it('re-attaches after a Restart without repeating what is on screen', () => {
    const panel = fakePanel()
    const view = follow(panel)
    panel.send('t1', 't2')
    panel.end()
    vi.advanceTimersByTime(1000)
    panel.send('t1', 't2', 't3 boot')
    expect(view.texts()).toEqual(['t1', 't2', 't3 boot'])
  })

  it('backs off while there is nothing to attach to, then resets once it is back', () => {
    const panel = fakePanel()
    const view = follow(panel)
    panel.send('t1')
    // Mid-swap: no container yet, so each attempt fails straight away.
    panel.end()
    vi.advanceTimersByTime(1000)
    panel.fail('bot-a has no container')
    vi.advanceTimersByTime(1999)
    expect(panel.opened).toHaveLength(2)
    vi.advanceTimersByTime(1)
    expect(panel.opened).toHaveLength(3)
    panel.fail('bot-a has no container')
    vi.advanceTimersByTime(4000)
    expect(panel.opened).toHaveLength(4)

    // The new container answers: the failure is cleared and the backoff resets.
    panel.send('t2 boot')
    expect(view.errors.at(-1)).toBeNull()
    expect(view.reconnecting.at(-1)).toBe(false)
    panel.end()
    vi.advanceTimersByTime(1000)
    expect(panel.opened).toHaveLength(5)
  })

  it('counts a stream that stays open as attached, even before a new line', () => {
    const panel = fakePanel()
    const view = follow(panel)
    panel.send('t1')
    panel.end()
    vi.advanceTimersByTime(1000)
    panel.fail('bot-a has no container')
    vi.advanceTimersByTime(2000)
    // A quiet bot: the replay only repeats the screen, then nothing.
    panel.send('t1')
    expect(view.reconnecting.at(-1)).toBe(true)
    vi.advanceTimersByTime(SETTLED_MS)
    expect(view.reconnecting.at(-1)).toBe(false)
    expect(view.errors.at(-1)).toBeNull()
  })

  it('does not retry a stopped bot', () => {
    const panel = fakePanel()
    const view = follow(panel, false)
    panel.send('t1')
    panel.end()
    vi.advanceTimersByTime(60_000)
    expect(panel.opened).toHaveLength(1)
    expect(view.reconnecting.at(-1)).toBe(false)
  })

  it('reports a failure on a stopped bot as an error', () => {
    const panel = fakePanel()
    const view = follow(panel, false)
    panel.fail('bot-a has no container')
    expect(view.errors.at(-1)).toBe('bot-a has no container')
    expect(view.reconnecting.at(-1)).toBe(false)
  })

  it('stops the stream and any pending retry', () => {
    const panel = fakePanel()
    const view = follow(panel)
    panel.send('t1')
    panel.end()
    view.stop()
    vi.advanceTimersByTime(60_000)
    expect(panel.opened).toHaveLength(1)
    expect(panel.opened[0]?.aborted).toBe(true)
  })

  it('lines the replay up against the buffer as it is at each attach', () => {
    const panel = fakePanel()
    let buffer: LogEntry[] = lines('t0 before mount')
    followLog({
      open: panel.open,
      replay: 500,
      live: true,
      current: () => buffer,
      onAppend: (entries) => {
        buffer = [...buffer, ...entries]
      },
      onError: () => {},
      onReconnecting: () => {},
    })
    panel.send('t0 before mount', 't1')
    expect(buffer.map((e) => e.text)).toEqual(['t0 before mount', 't1'])
  })
})

// A Server-Sent Events reader built on `fetch`, not `EventSource`.
//
// `EventSource` can only issue GETs, and the approve / dry-run routes are POSTs
// (they start a container, so they are not safe to be retried by a prefetcher).
// One reader for all three streams keeps the parsing in a single place.

import { notifyUnauthorized } from './api'

export interface SseHandlers {
  /** A named event with a parsed JSON payload. */
  onEvent: (event: string, data: unknown) => void
  /** The stream ended cleanly. */
  onDone?: () => void
  /** The stream failed. `abort()` does not call this. */
  onError?: (message: string) => void
}

export interface SseStream {
  /** Stop reading and release the connection. */
  abort: () => void
}

/**
 * Read an SSE stream until it ends or `abort` is called.
 *
 * Frames are `event: <name>` followed by one or more `data:` lines, terminated by
 * a blank line. Comment lines (`:`) are the server's keep-alive and are ignored.
 */
export function streamSse(
  url: string,
  init: RequestInit,
  handlers: SseHandlers,
): SseStream {
  const controller = new AbortController()

  void (async () => {
    try {
      const res = await fetch(url, {
        ...init,
        signal: controller.signal,
        headers: { ...init.headers, Accept: 'text/event-stream' },
      })
      if (!res.ok) {
        // Same rule as `api.request`: an expired password session on a stream
        // has to land the operator on the login screen, not leave them staring
        // at "not authorized" on a page whose buttons no longer work.
        if (res.status === 401 || res.status === 403) {
          notifyUnauthorized()
        }
        handlers.onError?.(await readError(res))
        return
      }
      if (!res.body) {
        handlers.onError?.('the panel returned no stream')
        return
      }

      const reader = res.body.getReader()
      const decoder = new TextDecoder()
      let buffer = ''

      for (;;) {
        const { done, value } = await reader.read()
        if (done) break
        buffer += decoder.decode(value, { stream: true })

        // Frames are separated by a blank line. Keep the trailing partial frame.
        const frames = buffer.split('\n\n')
        buffer = frames.pop() ?? ''
        for (const frame of frames) {
          dispatch(frame, handlers)
        }
      }
      // A stream can end with an unterminated final frame.
      if (buffer.trim() !== '') dispatch(buffer, handlers)
      handlers.onDone?.()
    } catch (e) {
      if (controller.signal.aborted) return
      handlers.onError?.((e as Error).message)
    }
  })()

  return { abort: () => controller.abort() }
}

function dispatch(frame: string, handlers: SseHandlers) {
  let event = 'message'
  const dataLines: string[] = []

  for (const line of frame.split('\n')) {
    if (line.startsWith(':') || line.trim() === '') continue
    if (line.startsWith('event:')) {
      event = line.slice(6).trim()
    } else if (line.startsWith('data:')) {
      dataLines.push(line.slice(5).trimStart())
    }
  }
  if (dataLines.length === 0) return

  const raw = dataLines.join('\n')
  try {
    handlers.onEvent(event, JSON.parse(raw))
  } catch {
    // A frame we can't parse is still worth showing rather than dropping.
    handlers.onEvent('error', { message: `unreadable event: ${raw}` })
  }
}

async function readError(res: Response): Promise<string> {
  try {
    const body = (await res.json()) as { error?: string }
    if (body.error) return body.error
  } catch {
    // Fall through.
  }
  return `${res.status} ${res.statusText}`
}

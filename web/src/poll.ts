// Polling that waits for each answer and stops when nobody is looking.
//
// Several screens re-read the server every few seconds, and some of those reads
// (a bot's funding) hit the operator's RPC. A plain setInterval kept firing in
// a background tab, and kept firing while the last read was still out, so a
// slow node stacked requests on top of each other. Here the next read is
// scheduled only after the last one settles, and none is sent while the page
// is hidden. Coming back to the tab reads at once, so what the operator sees on
// return is never a poll interval old.

/** Whether the page is visible, and a way to hear when that changes. */
export interface Visibility {
  hidden(): boolean
  /** Call `listener` on every change; returns the unsubscribe. */
  subscribe(listener: () => void): () => void
}

export const pageVisibility: Visibility = {
  hidden: () => document.visibilityState === 'hidden',
  subscribe: (listener) => {
    document.addEventListener('visibilitychange', listener)
    return () => document.removeEventListener('visibilitychange', listener)
  },
}

export interface PollOptions {
  /** Read once right away instead of waiting a full interval. */
  immediate?: boolean
  visibility?: Visibility
}

/**
 * Run `tick` every `intervalMs`, counted from when the previous run settled.
 * A rejected tick doesn't stop the poll; handle errors inside `tick` if they
 * matter. Returns the stop function, for a `useEffect` cleanup.
 */
export function poll(
  tick: () => Promise<unknown>,
  intervalMs: number,
  { immediate = false, visibility = pageVisibility }: PollOptions = {},
): () => void {
  let stopped = false
  let running = false
  let timer: ReturnType<typeof setTimeout> | undefined

  const schedule = () => {
    if (!stopped) timer = setTimeout(run, intervalMs)
  }

  const run = () => {
    timer = undefined
    if (stopped || running) return
    // Hidden: no read and no timer. The visibility listener restarts it.
    if (visibility.hidden()) return
    running = true
    tick()
      .catch(() => {})
      .finally(() => {
        running = false
        schedule()
      })
  }

  const unsubscribe = visibility.subscribe(() => {
    // Back in view with nothing pending: read now rather than an interval late.
    if (!visibility.hidden() && timer === undefined) run()
  })

  if (immediate) run()
  else schedule()

  return () => {
    stopped = true
    if (timer !== undefined) clearTimeout(timer)
    unsubscribe()
  }
}

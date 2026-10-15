/**
 * A generation guard for async work whose result writes back into a form.
 *
 * `begin()` starts a new generation and returns `stale()`, which turns true once
 * anything calls `begin()` or `invalidate()` again, or the owner closes the
 * guard (the component unmounted). A stale result has to be dropped, not
 * applied: its callbacks close over form state that no longer exists.
 */
export interface StaleGuard {
  begin(): () => boolean
  invalidate(): void
  close(): void
  open(): void
}

export function createStaleGuard(): StaleGuard {
  let generation = 0
  let isOpen = true
  return {
    begin() {
      const mine = ++generation
      return () => !isOpen || generation !== mine
    },
    invalidate() {
      generation += 1
    },
    close() {
      isOpen = false
    },
    open() {
      isOpen = true
    },
  }
}

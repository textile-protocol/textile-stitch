// Which bots' values the fleet list re-reads each round.
//
// A running bot's funding is served from the reads the bot just made, so
// polling it costs no RPC and stays at the list's pace. A stopped bot's
// funding goes to the chain, and its wallet only moves when someone sends to
// or withdraws from it. So a stopped bot is read once, again when it stops
// after running, and again when the tab comes back into view (the caller
// clears the set then), not every round.

/** The bots to read this round: every running one, and each stopped one not
 * yet read since it stopped. */
export function namesToRead(
  names: string[],
  running: ReadonlySet<string>,
  stoppedRead: ReadonlySet<string>,
): string[] {
  return names.filter((name) => running.has(name) || !stoppedRead.has(name))
}

/** The stopped bots already read, given what is running now: a bot that
 * started drops out, so the next time it stops it is read again. */
export function stillStopped(
  stoppedRead: ReadonlySet<string>,
  running: ReadonlySet<string>,
): Set<string> {
  return new Set([...stoppedRead].filter((name) => !running.has(name)))
}

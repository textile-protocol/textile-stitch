// The one question an operator has about a bot: is it quoting? Two things
// have to be true: the process is up, and Textile is sending it quotes. Docker
// (and the desktop app's process runtime) answer the first with seven
// container states and a free-text status line; the config answers the
// second. This folds both into one word.
//
//   live       — the process is up and Textile routes quotes to it.
//   waiting    — up, but not seated yet: the email isn't confirmed, no
//                corridor is assigned, or Connect never ran.
//   stopped    — not running, and nothing went wrong: never started, stopped
//                on purpose, paused.
//   crashed    — it died on its own: a non-zero exit, a signal that wasn't the
//                panel's own SIGTERM, Docker's dead, or a restart loop.
//
// The raw state and status stay on hover for anyone debugging.

import type { BotState, VenueSeat } from './types'

export type BotStatus = 'live' | 'waiting' | 'stopped' | 'crashed'

/** Docker: "Exited (137) 3 minutes ago". Process runtime: "Exited (exit status: 1)"
 *  or "Exited (signal: 15 (SIGTERM))". */
function exitLooksClean(status: string): boolean {
  const code = /exit(?:ed)?\s*(?:status:)?\s*\(?\s*(\d+)/i.exec(status)
  if (code) return code[1] === '0'
  const signal = /signal:\s*(\d+)/i.exec(status)
  // 15 is SIGTERM, which is what Stop sends. 2 is Ctrl-C. Anything else
  // (9 from the OOM killer, 11 for a segfault) is not a stop.
  if (signal) return signal[1] === '15' || signal[1] === '2'
  // No code at all ("Exited"): the process runtime saying it isn't up and
  // the operator wanted it up, most often right after a Stop. Not a crash.
  return true
}

export function botStatus(state: BotState, status: string, venue?: VenueSeat): BotStatus {
  switch (state) {
    case 'running':
      return venue === undefined || venue === 'seated' ? 'live' : 'waiting'
    case 'exited':
      return exitLooksClean(status) ? 'stopped' : 'crashed'
    case 'dead':
    case 'restarting':
      return 'crashed'
    case 'created':
    case 'paused':
    case 'unknown':
    default:
      return 'stopped'
  }
}

/** What the pill says on hover for a waiting bot, so amber comes with a reason. */
export function waitingReason(venue: VenueSeat): string {
  return venue === 'not-connected'
    ? 'Running, but not connected to Textile. Nobody can trade with it until it is.'
    : 'Running, but Textile is not sending it quotes yet: confirm the email Textile sent, or the maker has no corridor assigned. If you already confirmed, ask Textile whether this maker is blocked.'
}

// The Fund step's own state, as a pure reducer.
//
// The step has two jobs: watch the wallet until the server says the bot may
// start, then hand over to the shared start runner (`useStartSequence`). This
// reducer covers the first job and the hand-over; the runner keeps its own
// state. Kept pure so it can be unit-tested without React once a test runner
// exists in this package.

import type { Bot, Funding } from '../../types'

export type FundPhase =
  /** First read of the bot and its wallet. */
  | 'loading'
  /** Polling the wallet until the gate passes. */
  | 'checking'
  /** The start runner has the wheel. */
  | 'running'
  /** The bot no longer exists (404). */
  | 'gone'
  /** The panel can't read this bot's config (409): nothing to poll. */
  | 'unreadable'

export interface FundState {
  phase: FundPhase
  funding: Funding | null
  bot: Bot | null
  /** Poll interval in ms. Doubles on read errors, up to POLL_MAX_MS. */
  pollMs: number
  /** Last read failure, shown in a banner while polling continues. */
  loadError: string | null
  /** Unix ms of the last completed read, for the status line. */
  lastCheckedAt: number | null
}

export type FundAction =
  | { type: 'loaded'; bot: Bot; funding: Funding; at: number }
  | { type: 'funding'; funding: Funding; at: number }
  | { type: 'fetch-failed'; message: string; at: number }
  | { type: 'gone' }
  | { type: 'unreadable'; message: string }
  /** The gate passed and the runner was started. */
  | { type: 'run' }
  /** Retry from scratch: re-read everything from the server. */
  | { type: 'retry' }

export const POLL_MS = 5000
export const POLL_MAX_MS = 30_000

export const INITIAL_FUND: FundState = {
  phase: 'loading',
  funding: null,
  bot: null,
  pollMs: POLL_MS,
  loadError: null,
  lastCheckedAt: null,
}

export function reduceFund(state: FundState, action: FundAction): FundState {
  switch (action.type) {
    case 'loaded':
      return {
        ...state,
        phase: 'checking',
        bot: action.bot,
        funding: action.funding,
        pollMs: POLL_MS,
        loadError: null,
        lastCheckedAt: action.at,
      }
    case 'funding':
      return {
        ...state,
        funding: action.funding,
        pollMs: POLL_MS,
        loadError: null,
        lastCheckedAt: action.at,
      }
    case 'fetch-failed':
      return {
        ...state,
        // A first load that failed still moves to checking: the poll keeps
        // trying, and the rows say what they can.
        phase: state.phase === 'loading' ? 'checking' : state.phase,
        loadError: action.message,
        pollMs: Math.min(state.pollMs * 2, POLL_MAX_MS),
        lastCheckedAt: action.at,
      }
    case 'gone':
      return { ...state, phase: 'gone' }
    case 'unreadable':
      return { ...state, phase: 'unreadable', loadError: action.message }
    case 'run':
      return state.phase === 'checking' ? { ...state, phase: 'running' } : state
    case 'retry':
      return { ...state, phase: 'loading', loadError: null }
  }
}

/**
 * The sentences under the gate line, from the server's booleans. Gas only:
 * the token sides are not this screen's business (the bot page waits for
 * them), so an empty wallet with gas in it is simply ready.
 */
export function gateReasons(
  f: Funding,
  words: {
    cantRead: string
    needsGas: (minGas: number, gas: string) => string
    gasUnpriced: string
  },
): string[] {
  if (f.readError) return [words.cantRead]
  if (!f.gate.needsGas) return []
  // On a chain whose gas the panel can't price, "$1 of gas" is not an
  // instruction anyone can follow; the one that is says any balance counts.
  return f.gas.price === null && f.gas.balance !== null
    ? [words.gasUnpriced]
    : [words.needsGas(f.gate.minGasUsd, f.gas.symbol)]
}

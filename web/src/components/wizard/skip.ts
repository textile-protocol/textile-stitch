// Where the wizard's Skip link lands: decided by what is left to do, not by
// which step the link sits on. The same Approve screen can be waiting for gas
// (approvals still owed) or showing a start that failed after the approvals
// landed, and those two need different places on the bot page.
//
//   approve  Tools, with the handoff the bot page already reads after a
//            create: its Permit2 banner points at Approve allowances, then
//            Start.
//   connect  Corridors, where the Textile card connects the bot and sends and
//            confirms the email.
//   start    Funds, the bot page's own landing. Start and Restart are in the
//            header on every tab.

import { botPath } from '../../botRoutes'
import type { StartStage } from './useStartSequence'

export type LeftOver = 'approve' | 'connect' | 'start'

export interface SkipDestination {
  path: string
  /** Router state for the bot page, or null for none. */
  state: { needsPermit2: true } | null
}

/** What the start runner's failure stage leaves for the bot page. */
const AFTER_FAILURE: Record<StartStage, LeftOver> = {
  approve: 'approve',
  access: 'connect',
  start: 'start',
  verify: 'start',
}

/**
 * What a skip leaves undone. A failed run says it exactly; with no failure the
 * screen's own state decides (`fallback`): waiting for gas means approvals are
 * still owed, waiting for the email means Textile is.
 */
export function leftOver(failed: StartStage | null, fallback: LeftOver): LeftOver {
  return failed ? AFTER_FAILURE[failed] : fallback
}

export function skipDestination(bot: string, left: LeftOver): SkipDestination {
  switch (left) {
    case 'approve':
      return { path: botPath(bot, 'tools'), state: { needsPermit2: true } }
    case 'connect':
      return { path: botPath(bot, 'settings'), state: null }
    case 'start':
      return { path: botPath(bot, 'funds'), state: null }
  }
}

// Where the wizard left off, so a closed tab or a reload can reopen the Fund
// step for the bot that was being set up.
//
// Two records, one per lane: the new-bot one here, and the add-lane one at the
// foot of the file. Only the bot's name is stored for the new-bot lane. Nothing
// about progress lives in either: on remount
// the Fund step asks the server (bot state, balances, allowances, RFQ access)
// and lands in the right phase from that. The record is cleared as soon as the
// wizard reaches either of its endings (running, or waiting for Textile), and
// a record nobody cleared expires on its own, so an abandoned run can never
// hold /add hostage: a stale name would reopen the Fund step for the old bot
// and there would be no way to set up a second one.

const KEY = 'stitch-wizard-resume'

/** A record older than this is ignored. A wizard run does not span a day. */
const MAX_AGE_MS = 24 * 60 * 60 * 1000

export interface ResumeRecord {
  bot: string
  /** Unix ms when the record was written. */
  at: number
}

export function saveResume(bot: string): void {
  try {
    const record: ResumeRecord = { bot, at: Date.now() }
    window.localStorage.setItem(KEY, JSON.stringify(record))
  } catch {
    // Private mode or storage disabled: the `?resume=` query still works.
  }
}

export function readResume(): ResumeRecord | null {
  try {
    const raw = window.localStorage.getItem(KEY)
    if (!raw) return null
    const parsed = JSON.parse(raw) as Partial<ResumeRecord>
    if (typeof parsed.bot !== 'string' || parsed.bot === '') return null
    const at = typeof parsed.at === 'number' ? parsed.at : 0
    // An old record is a run nobody finished. Forget it rather than reopen it.
    if (Date.now() - at > MAX_AGE_MS) {
      clearResume()
      return null
    }
    return { bot: parsed.bot, at }
  } catch {
    return null
  }
}

export function clearResume(): void {
  try {
    window.localStorage.removeItem(KEY)
  } catch {
    // Nothing to clear, or storage disabled.
  }
}

/**
 * The add lane's own record, for the window between the pool landing on disk
 * and Textile being told about it.
 *
 * `addPool` writes the corridor and restarts the bot; `enrollRfq` runs a moment
 * later. In between, a closed tab leaves a pool with an empty `rfq_corridor`,
 * which means the bot answers nothing on that corridor while the fleet page
 * shows it healthy, and nothing in the panel ever says so. The URL alone
 * recovers a reload of the same tab; this recovers everything else.
 *
 * It covers that window and nothing more: it is written when the pool lands and
 * cleared the moment enrolment succeeds, not at the end of the flow. What comes
 * after (funding, approving, starting) leaves no broken state behind and can
 * take as long as money takes to arrive, and a record standing that long would
 * reopen this lane on every later visit to /add.
 */
const ADD_KEY = 'stitch-wizard-add'

export interface AddResumeRecord {
  bot: string
  corridorId: string
  /** The pool `addPool` returned, so a rebuilt lane never adds it twice. */
  poolIndex: number
  at: number
}

export function saveAddResume(
  bot: string,
  corridorId: string,
  poolIndex: number,
): void {
  try {
    const record: AddResumeRecord = { bot, corridorId, poolIndex, at: Date.now() }
    window.localStorage.setItem(ADD_KEY, JSON.stringify(record))
  } catch {
    // Private mode or storage disabled: the URL still recovers this tab.
  }
}

export function readAddResume(): AddResumeRecord | null {
  try {
    const raw = window.localStorage.getItem(ADD_KEY)
    if (!raw) return null
    const parsed = JSON.parse(raw) as Partial<AddResumeRecord>
    if (typeof parsed.bot !== 'string' || parsed.bot === '') return null
    if (typeof parsed.corridorId !== 'string' || parsed.corridorId === '') return null
    if (!Number.isInteger(parsed.poolIndex) || (parsed.poolIndex ?? -1) < 0) return null
    const at = typeof parsed.at === 'number' ? parsed.at : 0
    if (Date.now() - at > MAX_AGE_MS) {
      clearAddResume()
      return null
    }
    return {
      bot: parsed.bot,
      corridorId: parsed.corridorId,
      poolIndex: parsed.poolIndex as number,
      at,
    }
  } catch {
    return null
  }
}

export function clearAddResume(): void {
  try {
    window.localStorage.removeItem(ADD_KEY)
  } catch {
    // Nothing to clear, or storage disabled.
  }
}

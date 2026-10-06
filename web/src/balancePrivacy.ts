// Whether balances are masked, the way a banking app hides them behind dots.
// One switch for the whole panel, kept across visits so a panel opened on a
// shared screen or a call doesn't flash the numbers before it's flipped back.
//
// The fleet list, the bot page header and the Funds tab all read the same
// flag, so clicking any balance masks every one of them. Other tabs follow
// through the `storage` event, which is listened to for the life of the page
// rather than per subscriber: a change made while no balance is on screen
// (say, on the add-corridor page) must still be seen on the way back.

import { useSyncExternalStore } from 'react'

const KEY = 'stitch-panel-hide-balances'

/** What a masked balance reads as where it has to stay plain text (a select
 * option, an input). A fixed length, so the mask doesn't give away the size. */
export const MASK = '•••••'

/** The saved choice. Anything unreadable reads as shown. */
export function readBalancesHidden(): boolean {
  try {
    return window.localStorage.getItem(KEY) === '1'
  } catch {
    return false
  }
}

export function saveBalancesHidden(hidden: boolean): void {
  try {
    window.localStorage.setItem(KEY, hidden ? '1' : '0')
  } catch {
    // Private mode or storage disabled: the choice lasts for this visit only.
  }
}

/** `text`, or the mask when balances are hidden. */
export function maskBalance(text: string, hidden: boolean): string {
  return hidden ? MASK : text
}

// The live flag. Read lazily, so nothing touches storage at import time, and
// held in memory so the switch still works when storage refuses the write.
let current: boolean | null = null
let listening = false
const listeners = new Set<() => void>()

/** Another tab changed the setting. */
function onStorage(e: StorageEvent): void {
  if (e.key !== KEY) return
  current = e.newValue === '1'
  listeners.forEach((l) => l())
}

/** Installed on first use and never removed, so the cache can't drift from
 * storage while nothing is subscribed. */
function listen(): void {
  if (listening) return
  listening = true
  window.addEventListener('storage', onStorage)
}

export function balancesHidden(): boolean {
  listen()
  if (current === null) current = readBalancesHidden()
  return current
}

export function setBalancesHidden(hidden: boolean): void {
  listen()
  current = hidden
  saveBalancesHidden(hidden)
  listeners.forEach((l) => l())
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

/** The masked flag and a toggle that flips it for every balance in the panel. */
export function useBalancesHidden(): [boolean, () => void] {
  const hidden = useSyncExternalStore(subscribe, balancesHidden)
  return [hidden, () => setBalancesHidden(!balancesHidden())]
}

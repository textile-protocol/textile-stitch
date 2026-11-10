// The fleet filter, remembered across visits and shared between tabs.
//
// The URL stays the filter for this tab (back from a bot, a bookmark, a link),
// and every change is also saved to localStorage. A visit whose URL names no
// filter picks up the saved one, so leaving the page and coming back, or
// opening the panel in a new tab, shows the list the way it was left. A tab
// that is already open follows a change made in another one.

import { useCallback, useEffect, useState } from 'react'
import { useSearchParams } from 'react-router-dom'
import {
  type FleetFilter,
  readFilter,
  resolveFilter,
  savedForm,
  writeFilter,
} from './fleetFilter'

const KEY = 'stitch-fleet-filter'

function readSaved(): string | null {
  try {
    return window.localStorage.getItem(KEY)
  } catch {
    return null
  }
}

function save(filter: FleetFilter): void {
  try {
    window.localStorage.setItem(KEY, savedForm(filter))
  } catch {
    // Private mode or storage disabled: the filter lasts as long as the URL.
  }
}

export function useFleetFilter(): [FleetFilter, (next: FleetFilter) => void] {
  const [params, setParams] = useSearchParams()
  const [saved, setSaved] = useState(readSaved)

  // `storage` only fires in the other tabs, so this never echoes its own save.
  // The other tab's choice replaces this tab's, keeping this tab's search.
  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key !== KEY) return
      setSaved(e.newValue)
      setParams(
        (prev) =>
          writeFilter({
            ...readFilter(new URLSearchParams(e.newValue ?? '')),
            query: prev.get('q') ?? '',
          }),
        { replace: true },
      )
    }
    window.addEventListener('storage', onStorage)
    return () => window.removeEventListener('storage', onStorage)
  }, [setParams])

  const setFilter = useCallback(
    (next: FleetFilter) => {
      save(next)
      setSaved(savedForm(next))
      // `replace`, so typing in the search box doesn't stack a history entry a key.
      setParams(writeFilter(next), { replace: true })
    },
    [setParams],
  )

  return [resolveFilter(params, saved), setFilter]
}

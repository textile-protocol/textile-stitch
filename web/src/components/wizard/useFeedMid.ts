// The live mid behind the Spread step's worked example.
//
// One fetch per feed URL, through the panel. The example is meant to show the
// operator what their bps do to the price the bot will actually quote around,
// so this reads the same URL the Sources step just settled on, Textile's or
// their own. A feed that doesn't answer is a state here, not an error: the
// example falls back to a round notional and says so, and Next stays enabled,
// because a spread in bps is right whatever the mid turns out to be.

import { useEffect, useState } from 'react'
import { api, type FeedMid } from '../../api'

export type FeedMidState =
  | { status: 'idle' }
  | { status: 'loading' }
  | { status: 'ok'; mid: FeedMid }
  | { status: 'down'; reason: string }

export function useFeedMid(url: string | null): FeedMidState {
  const [state, setState] = useState<FeedMidState>({ status: 'idle' })

  useEffect(() => {
    const target = url?.trim() ?? ''
    if (!/^https?:\/\//.test(target)) {
      setState({ status: 'idle' })
      return
    }
    const controller = new AbortController()
    setState({ status: 'loading' })
    api
      .feedMid(target, controller.signal)
      .then((mid) => {
        if (!controller.signal.aborted) setState({ status: 'ok', mid })
      })
      .catch((e: unknown) => {
        if (controller.signal.aborted) return
        setState({
          status: 'down',
          reason: e instanceof Error ? e.message : String(e),
        })
      })
    return () => controller.abort()
  }, [url])

  return state
}

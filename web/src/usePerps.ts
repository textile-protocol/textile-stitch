import { useCallback, useEffect, useRef, useState } from 'react'
import { api, type HedgeMode, type PerpsView } from './api'
import { poll } from './poll'

type Changes = Partial<
  Pick<PerpsView, 'assignmentId' | 'recallEnabled' | 'fundingTargetAtoms'>
>
type State = {
  name: string
  view: PerpsView | null
  error: string | null
  busy: boolean
  loading: boolean
}
const initial = (name: string): State => ({
  name,
  view: null,
  error: null,
  busy: false,
  loading: true,
})

/** One poll and one mutation lock shared by Experimental and the module. */
export function usePerps(name: string) {
  const [state, setState] = useState(() => initial(name))
  const owner = useRef(name)
  owner.current = name
  const version = useRef(0)
  const lifetime = useRef(0)
  const writing = useRef(false)

  const refresh = useCallback(async () => {
    const request = ++version.current
    try {
      const view = await api.perps(name)
      if (owner.current !== name || request !== version.current) return false
      setState((s) => ({ ...s, name, view, loading: false, error: null }))
      return true
    } catch (e) {
      if (owner.current !== name || request !== version.current) return false
      setState((s) => ({ ...s, loading: false, error: String(e) }))
      return false
    }
  }, [name])

  useEffect(() => {
    lifetime.current++
    setState(initial(name))
    writing.current = false
    const stop = poll(
      async () => {
        if (!writing.current) await refresh()
      },
      5000,
      { immediate: true }
    )
    return () => {
      stop()
      version.current++
      lifetime.current++
    }
  }, [name, refresh])

  const mutate = async (action: () => Promise<unknown>) => {
    if (writing.current || owner.current !== name) return false
    writing.current = true
    const generation = lifetime.current
    const active = () =>
      owner.current === name && generation === lifetime.current
    version.current++ // Discard reads started before the operator's change.
    setState((s) => ({ ...s, busy: true, error: null }))
    try {
      await action()
      if (!active()) return false
      const refreshed = await refresh()
      return refreshed && active()
    } catch (e) {
      if (active()) setState((s) => ({ ...s, error: String(e) }))
      return false
    } finally {
      if (active()) {
        writing.current = false
        setState((s) => ({ ...s, busy: false }))
      }
    }
  }
  const current = state.name === name ? state : initial(name)
  return {
    ...current,
    refresh,
    save: (mode: HedgeMode, changes: Changes = {}) => {
      const view = current.view
      if (!view) return Promise.resolve(false)
      return mutate(() =>
        api.setPerps(
          name,
          changes.assignmentId ?? view.assignmentId,
          mode,
          changes.recallEnabled ?? view.recallEnabled,
          changes.fundingTargetAtoms ?? view.fundingTargetAtoms
        )
      )
    },
    resume: (diagnosis: string) =>
      mutate(() => api.resumeRecall(name, diagnosis)),
  }
}
export type PerpsState = ReturnType<typeof usePerps>

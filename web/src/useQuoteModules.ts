import { useEffect, useRef, useState } from 'react'
import { api } from './api'
import {
  draftUnwindCapLimit,
  moduleParameterErrors,
} from './modulePresentation'
import type { ModulesConfig, ModulesView } from './modules'
import { poll } from './poll'

/** One revision-checked draft for all quote modules, independent of hedge mode. */
export function useQuoteModules(
  name: string,
  enabled: boolean,
  editable: boolean
) {
  const [view, setView] = useState<ModulesView | null>(null)
  const [draft, setDraft] = useState<ModulesConfig | null>(null)
  const [revision, setRevision] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [note, setNote] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const generation = useRef(0)
  const saving = useRef(false)

  useEffect(() => {
    let cancelled = false
    if (!enabled) return
    const stop = poll(
      async () => {
        if (saving.current) return
        const version = generation.current
        try {
          const next = await api.modules(name)
          if (cancelled || version !== generation.current) return
          setView(next)
          setDraft((current) => current ?? next.config)
          setRevision((current) => current || next.revision)
        } catch (e) {
          if (!cancelled && version === generation.current) setError(String(e))
        }
      },
      5000,
      { immediate: true }
    )
    return () => {
      cancelled = true
      stop()
    }
  }, [name, enabled])

  async function save() {
    if (
      !enabled ||
      !view ||
      !draft ||
      !editable ||
      saving.current ||
      moduleParameterErrors(
        draft,
        view.vault ?? true,
        draftUnwindCapLimit(view, draft, Date.now() / 1000)
      ).length
    )
      return
    saving.current = true
    generation.current += 1
    setBusy(true)
    setError(null)
    setNote(null)
    try {
      const result = await api.saveModules(name, revision, draft)
      const next = await api.modules(name)
      setView(next)
      setDraft(next.config)
      setRevision(next.revision)
      setNote(result.message)
      if (result.restartError)
        setError(
          `Configuration saved, but restart failed: ${result.restartError}`
        )
    } catch (e) {
      setError(String(e))
    } finally {
      saving.current = false
      setBusy(false)
    }
  }

  return {
    view,
    draft,
    setDraft,
    error,
    setError,
    note,
    setNote,
    busy,
    save,
    changed: Boolean(
      view && draft && JSON.stringify(draft) !== JSON.stringify(view.config)
    ),
    discard: () => {
      if (!view) return
      setDraft(view.config)
      setRevision(view.revision)
    },
  }
}
export type QuoteModulesState = ReturnType<typeof useQuoteModules>

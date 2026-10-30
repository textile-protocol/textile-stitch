import { useEffect, useRef, useState } from 'react'
import { api } from './api'
import { moduleParameterErrors } from './modulePresentation'
import type { ModulesConfig, ModulesView } from './modules'
import { expandModes, normalizeModes, sameConfig } from './moduleModes'
import { poll } from './poll'
import {
  quoteConfigForScope,
  remainingQuoteDraft,
  type QuoteConfigScope,
} from './moduleWorkspace'

/** One revision-checked draft for all quote modules, independent of hedge
 * mode. `base` and `draft` name every running module's mode
 * (`expandModes`), so saving one module never moves another's; `view` keeps
 * the config as the server sent it. */
export function useQuoteModules(
  name: string,
  enabled: boolean,
  editable: boolean
) {
  const [view, setView] = useState<ModulesView | null>(null)
  const [draft, setDraft] = useState<ModulesConfig | null>(null)
  const [base, setBase] = useState<Pick<
    ModulesView,
    'config' | 'revision'
  > | null>(null)
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
          setDraft((current) => current ?? expandModes(next.config))
          setBase(
            (current) =>
              current ?? {
                config: expandModes(next.config),
                revision: next.revision,
              }
          )
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

  const configFor = (scope: QuoteConfigScope) =>
    base && draft ? quoteConfigForScope(base.config, draft, scope) : null

  async function save(scope: QuoteConfigScope) {
    const config = configFor(scope)
    if (
      !enabled ||
      !view ||
      !draft ||
      !base ||
      !config ||
      !editable ||
      saving.current ||
      moduleParameterErrors(config, view.vault ?? true).length
    )
      return
    saving.current = true
    generation.current += 1
    setBusy(true)
    setError(null)
    setNote(null)
    try {
      // The server saves the same normalized shape; sending it keeps the
      // check below exact.
      const sent = normalizeModes(config)
      const result = await api.saveModules(name, base.revision, sent)
      const next = await api.modules(name)
      setView(next)
      // Another writer may have changed a sibling between PUT and GET. Do not
      // grant our retained drafts that writer's revision and overwrite it later.
      if (!sameConfig(next.config, sent)) {
        setError(
          'Configuration changed again while saving. Reload to review saved settings; unsaved edits were kept.'
        )
        return
      }
      const saved = expandModes(next.config)
      setDraft(remainingQuoteDraft(base.config, draft, saved, scope))
      setBase({ config: saved, revision: next.revision })
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
    base,
    draft,
    configFor,
    setDraft,
    error,
    setError,
    note,
    setNote,
    busy,
    save,
    discard: (scope: QuoteConfigScope) => {
      if (!base) return
      setDraft(
        (current) => current && quoteConfigForScope(current, base.config, scope)
      )
    },
  }
}
export type QuoteModulesState = ReturnType<typeof useQuoteModules>

import { useEffect, useRef, useState } from 'react'
import { api } from './api'
import { moduleParameterErrors } from './modulePresentation'
import type { Mode, ModulesConfig, ModulesView } from './modules'
import {
  expandModes,
  normalizeModes,
  sameConfig,
  withModuleMode,
  type ModuleKey,
} from './moduleModes'
import { poll } from './poll'
import {
  draftAfterModeSave,
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

  /** One module's Settings, through its save bar. */
  const save = (scope: QuoteConfigScope) => {
    const config = configFor(scope)
    return config
      ? commit(config, (saved, current, next) =>
          remainingQuoteDraft(saved, current, next, scope)
        )
      : Promise.resolve()
  }

  /** Only `key`'s mode, from the module header. Other edits stay drafts. */
  const saveMode = (key: ModuleKey, mode: Mode) =>
    base
      ? commit(withModuleMode(base.config, key, mode), (saved, current, next) =>
          draftAfterModeSave(saved, current, next, key, mode)
        )
      : Promise.resolve()

  async function commit(
    config: ModulesConfig,
    nextDraft: (
      saved: ModulesConfig,
      draft: ModulesConfig,
      next: ModulesConfig
    ) => ModulesConfig
  ) {
    if (
      !enabled ||
      !view ||
      !draft ||
      !base ||
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
      setDraft(nextDraft(base.config, draft, saved))
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
    saveMode,
    discard: (scope: QuoteConfigScope) => {
      if (!base) return
      setDraft(
        (current) => current && quoteConfigForScope(current, base.config, scope)
      )
    },
  }
}
export type QuoteModulesState = ReturnType<typeof useQuoteModules>

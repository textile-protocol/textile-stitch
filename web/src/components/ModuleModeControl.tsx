import { useEffect, useRef, useState } from 'react'
import {
  leanConflict,
  modeLabel,
  moduleModeEffect,
  moduleModes,
  moduleNames,
  moduleParameterErrors,
} from '../modulePresentation'
import { moduleMode, withModuleMode, type ModuleKey } from '../moduleModes'
import type { Mode } from '../modules'
import { quoteModuleUnsaved } from '../moduleWorkspace'
import type { QuoteModulesState } from '../useQuoteModules'
import { ModeSwitch } from './ModuleControls'
import { Button } from './ui'

/** A module's Off / Preview / Live switch in its header, reachable from every
 * section. Picking a mode asks first, then saves only that mode: it restarts
 * a running bot and can make the module act, so a stray click shouldn't. */
export default function ModuleModeControl({
  state,
  module,
  editable,
  refusal,
}: {
  state: QuoteModulesState
  module: ModuleKey
  editable: boolean
  /** Why this module can't be turned on here (the liquidity schedule without
   * a yield adapter), or null. */
  refusal: string | null
}) {
  const { view, base, draft, busy, saveMode } = state
  const [pending, setPending] = useState<Mode | null>(null)
  const root = useRef<HTMLDivElement>(null)
  useEffect(() => {
    if (pending === null) return
    const close = (e: Event) => {
      if (
        e instanceof KeyboardEvent
          ? e.key === 'Escape'
          : !root.current?.contains(e.target as Node)
      )
        setPending(null)
    }
    document.addEventListener('mousedown', close)
    document.addEventListener('keydown', close)
    return () => {
      document.removeEventListener('mousedown', close)
      document.removeEventListener('keydown', close)
    }
  }, [pending])
  if (!view || !base || !draft) return null
  const vault = view.vault ?? true
  const saved = moduleMode(base.config, module)
  // Off with the other inventory module on, or nothing to act through: the
  // reason sits in Settings too.
  const blocked =
    saved === 'off'
      ? (leanConflict(base.config, module, vault) ?? refusal)
      : null
  const next = pending && withModuleMode(base.config, module, pending)
  const errors = next ? moduleParameterErrors(next, vault) : []
  const unsaved = quoteModuleUnsaved(
    { ...view, config: base.config },
    draft,
    module
  )

  return (
    <div ref={root} className="relative">
      <fieldset
        disabled={!editable || busy || blocked !== null}
        title={blocked ?? undefined}
      >
        <ModeSwitch
          label={`${moduleNames[module]} mode`}
          modes={moduleModes}
          value={pending ?? saved}
          onChange={(mode) => setPending(mode === saved ? null : mode)}
        />
      </fieldset>
      {pending && next && (
        <div
          role="dialog"
          aria-label={`Set ${moduleNames[module]} to ${modeLabel(pending)}`}
          className="border-line-soft bg-surface absolute right-0 top-full z-20 mt-2 w-80 max-w-[calc(100vw-2rem)] space-y-3 rounded-xl border p-4 shadow-lg"
        >
          <p className="text-sm font-bold">
            Set {moduleNames[module].toLowerCase()} to {modeLabel(pending)}?
          </p>
          <p className="text-muted text-xs">
            {moduleModeEffect(module, pending, next)}
          </p>
          {errors.length > 0 && (
            <p role="alert" className="text-xs text-warning">
              {errors.join(' ')}
            </p>
          )}
          <p className="text-faint text-xs">
            Saving restarts a running bot; a stopped bot stays stopped.
            {unsaved && ' Your unsaved settings for this module stay unsaved.'}
          </p>
          <div className="flex gap-2">
            <Button
              variant="primary"
              busy={busy}
              disabled={errors.length > 0}
              onClick={() =>
                void saveMode(module, pending).then(() => setPending(null))
              }
            >
              Set to {modeLabel(pending)}
            </Button>
            <Button variant="ghost" onClick={() => setPending(null)}>
              Cancel
            </Button>
          </div>
        </div>
      )}
    </div>
  )
}

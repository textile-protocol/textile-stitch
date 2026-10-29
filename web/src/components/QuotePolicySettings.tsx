import {
  enforcedUnwindCapLimit,
  moduleModes,
  moduleNames,
  moduleParameterErrors,
  modulesFor,
} from '../modulePresentation'
import type { QuoteModulesState } from '../useQuoteModules'
import { ModeSwitch } from './ModuleControls'
import { Banner, Button } from './ui'

/** A deliberate group-wide action, outside every individual module editor. */
export default function QuotePolicySettings({
  state,
  editable,
}: {
  state: QuoteModulesState
  editable: boolean
}) {
  const { view, base, draft, setDraft, busy, configFor, save, discard } = state
  const config = configFor('policy')
  if (!view || !base || !draft || !config) return null
  const affected = modulesFor(view.vault ?? true).filter(
    (key) => base.config[key].enabled
  )
  const changed = draft.mode !== base.config.mode
  const errors = moduleParameterErrors(
    config,
    view.vault ?? true,
    enforcedUnwindCapLimit(view, base.config, config, Date.now() / 1000)
  )
  return (
    <details className="border-line-soft bg-surface rounded-xl border px-4 py-3">
      <summary className="cursor-pointer text-sm font-bold">
        Shared quote policy ·{' '}
        {moduleModes.find((mode) => mode.value === view.config.mode)?.label}
        {changed && <span className="ml-2 text-warning">Unsaved</span>}
      </summary>
      <div className="mt-4 space-y-4">
        <p className="text-muted text-sm">
          This mode applies to all enabled quote modules:{' '}
          {affected.length
            ? affected.map((key) => moduleNames[key]).join(', ')
            : 'none enabled'}
          . FX hedge has its own mode. Module enable switches and unsaved module
          edits stay unchanged.
        </p>
        <fieldset disabled={busy || !editable}>
          <ModeSwitch
            modes={moduleModes}
            value={draft.mode}
            onChange={(mode) => setDraft({ ...draft, mode })}
          />
        </fieldset>
        <p className="text-muted text-xs">
          {moduleModes.find((mode) => mode.value === draft.mode)?.effect}
        </p>
        {changed && errors.length > 0 && (
          <Banner tone="warning">{errors.join(' ')}</Banner>
        )}
        <div className="flex flex-wrap items-center gap-3">
          <Button
            variant="primary"
            busy={busy}
            disabled={!editable || !changed || errors.length > 0}
            onClick={() => void save('policy')}
          >
            Save quote policy
          </Button>
          {changed && (
            <Button
              variant="ghost"
              disabled={busy}
              onClick={() => discard('policy')}
            >
              Discard policy change
            </Button>
          )}
          <p className="text-muted text-xs">
            Saving restarts a running bot. A stopped bot stays stopped.
          </p>
        </div>
      </div>
    </details>
  )
}

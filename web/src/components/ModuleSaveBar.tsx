import type { ModulesConfig } from '../modules'
import { moduleChanges } from '../modulePresentation'
import { Button } from './ui'

/** Sticky save controls for Parameters. Quiet while the draft matches the
 * saved config; once it differs, lists each change as saved → draft so a
 * preset's changes to secondary settings are visible before saving. */
export default function ModuleSaveBar({
  saved,
  draft,
  valid,
  editable,
  busy,
  onDiscard,
  onSave,
}: {
  saved: ModulesConfig
  draft: ModulesConfig
  valid: boolean
  editable: boolean
  busy: boolean
  onDiscard: () => void
  onSave: () => void
}) {
  const dirty = JSON.stringify(draft) !== JSON.stringify(saved)
  const changes = dirty ? moduleChanges(saved, draft) : []
  const consequence = editable
    ? 'Saving restarts a running bot. A stopped bot stays stopped.'
    : 'Read-only access. You can explore a draft and simulate it, but cannot save.'
  return (
    <div
      className={`bg-surface z-10 flex flex-wrap items-center gap-x-4 gap-y-3 rounded-xl border px-4 py-3 shadow-sm sm:sticky sm:bottom-3 ${dirty ? 'border-accent' : 'border-line-soft'}`}
    >
      <div className="min-w-[16rem] flex-1">
        <p className="text-sm font-bold" aria-live="polite">
          {!valid
            ? 'Check your settings before continuing'
            : !dirty
              ? 'No unsaved changes'
              : changes.length === 0
                ? 'Unsaved changes'
                : `${changes.length} unsaved ${changes.length === 1 ? 'change' : 'changes'}`}
        </p>
        {changes.length > 0 && (
          <ul
            aria-label="Changes to save"
            className="mt-1.5 flex flex-wrap gap-1.5"
          >
            {changes.map((change) => (
              <li
                key={change.label}
                className="bg-canvas ring-line-soft rounded-md px-2 py-0.5 text-xs tabular-nums ring-1"
              >
                <span className="text-muted">{change.label}</span>{' '}
                <del className="text-muted">{change.from}</del>{' '}
                <span aria-hidden className="text-muted">
                  →
                </span>
                <span className="sr-only">to</span> <strong>{change.to}</strong>
              </li>
            ))}
          </ul>
        )}
        <p className="text-muted mt-1 text-xs">{consequence}</p>
      </div>
      <div className="flex flex-wrap gap-2">
        {dirty && (
          <Button disabled={busy} variant="ghost" onClick={onDiscard}>
            Discard
          </Button>
        )}
        <Button
          variant="primary"
          busy={busy}
          disabled={!editable || !dirty || !valid}
          onClick={onSave}
        >
          {draft.mode === 'live' ? 'Save & apply live' : 'Save settings'}
        </Button>
      </div>
    </div>
  )
}

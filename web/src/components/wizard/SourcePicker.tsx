// One source: Textile's default (shown, so the choice is informed) or your own.
//
// Shared by the new-bot Sources step, which asks about the price feed and the
// RPC, and the add-a-corridor Price feed step, which asks about the feed alone:
// the RPC belongs to the bot, not to one of its corridors.

import { Input } from '../ui'

export const isHttpUrl = (s: string) => /^https?:\/\/\S+$/i.test(s.trim())

/** A source is settled when the default exists, or the typed URL is a URL. */
export function sourceOk(
  mode: 'default' | 'own',
  defaultUrl: string,
  ownUrl: string,
): boolean {
  return mode === 'default' ? defaultUrl.trim() !== '' : isHttpUrl(ownUrl)
}

export default function SourcePicker({
  label,
  hint,
  mode,
  defaultUrl,
  url,
  onMode,
  onUrl,
}: {
  label: string
  hint: string
  mode: 'default' | 'own'
  defaultUrl: string
  url: string
  onMode: (mode: 'default' | 'own') => void
  onUrl: (url: string) => void
}) {
  const hasDefault = defaultUrl.trim() !== ''
  const row = (active: boolean, disabled = false) =>
    `flex items-start gap-3 rounded-lg border p-3 ${
      disabled
        ? 'cursor-not-allowed border-line-soft opacity-50'
        : active
          ? 'cursor-pointer border-accent bg-accent-tint'
          : 'cursor-pointer border-line-soft hover:bg-hover'
    }`
  return (
    <div className="block">
      <p className="mb-1 text-sm">
        <span className="font-bold">{label}</span>
        <span className="text-muted"> - {hint}</span>
      </p>
      <div className="space-y-2">
        <label className={row(mode === 'default', !hasDefault)}>
          <input
            type="radio"
            name={`source-${label}`}
            checked={mode === 'default'}
            disabled={!hasDefault}
            onChange={() => onMode('default')}
            className="mt-1 accent-[var(--tx-accent)]"
          />
          <span className="min-w-0">
            <span className="block font-bold">Textile default</span>
            <span className="block break-all font-mono text-xs text-muted">
              {hasDefault ? defaultUrl : 'No default for a custom corridor'}
            </span>
          </span>
        </label>
        <label className={row(mode === 'own')}>
          <input
            type="radio"
            name={`source-${label}`}
            checked={mode === 'own'}
            onChange={() => onMode('own')}
            className="mt-1 accent-[var(--tx-accent)]"
          />
          <span className="min-w-0 flex-1">
            <span className="block font-bold">My own</span>
            {mode === 'own' && (
              <Input
                value={url}
                placeholder="https://…"
                onChange={(e) => onUrl(e.target.value)}
              />
            )}
          </span>
        </label>
      </div>
    </div>
  )
}

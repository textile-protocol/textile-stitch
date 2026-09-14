// The pencil next to the bot's name. Click, type, Enter. Escape cancels; an
// empty name goes back to the wallet id. The id itself never changes: this is
// a label the panel keeps beside the config, nothing the bot or the venue
// sees.

import { useEffect, useRef, useState } from 'react'
import { ApiError, api } from '../api'
import type { Bot } from '../types'

const MAX = 40

export default function RenameBot({ bot, onRenamed }: { bot: Bot; onRenamed: (bot: Bot) => void }) {
  const [editing, setEditing] = useState(false)
  const [value, setValue] = useState(bot.displayName ?? '')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const inputRef = useRef<HTMLInputElement>(null)

  useEffect(() => {
    if (editing) inputRef.current?.focus()
  }, [editing])

  function start() {
    setValue(bot.displayName ?? '')
    setError(null)
    setEditing(true)
  }

  function cancel() {
    setEditing(false)
    setError(null)
  }

  async function save() {
    const next = value.trim()
    if (next === (bot.displayName ?? '')) {
      cancel()
      return
    }
    setBusy(true)
    setError(null)
    try {
      const updated = await api.rename(bot.name, next)
      onRenamed(updated)
      setEditing(false)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  if (!editing) {
    return (
      <button
        type="button"
        onClick={start}
        title="Rename this bot (the wallet id stays)"
        aria-label="Rename this bot"
        className="rounded p-1 text-muted hover:bg-hover hover:text-ink"
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden>
          <path d="M12 20h9" />
          <path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z" />
        </svg>
      </button>
    )
  }

  return (
    <form
      className="flex flex-wrap items-center gap-2"
      onSubmit={(e) => {
        e.preventDefault()
        void save()
      }}
    >
      <input
        ref={inputRef}
        value={value}
        maxLength={MAX}
        placeholder={bot.name}
        disabled={busy}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Escape') cancel()
        }}
        className="w-56 rounded-lg border border-line bg-surface px-2 py-1 text-base font-bold text-ink outline-none focus:border-accent"
      />
      <button
        type="submit"
        disabled={busy}
        className="rounded-lg bg-accent px-2.5 py-1 text-sm font-bold text-on-accent disabled:opacity-60"
      >
        Save
      </button>
      <button type="button" onClick={cancel} disabled={busy} className="text-sm text-muted hover:text-ink">
        Cancel
      </button>
      {error && <span className="text-xs text-danger">{error}</span>}
    </form>
  )
}

import { useEffect, useState } from 'react'
import { ApiError, api } from '../api'
import { Banner, Button, ErrorState, Loading, TextArea } from './ui'

/**
 * The escape hatch: edit `stitch.toml` directly.
 *
 * The form covers the fields most operators touch; a pool config has around
 * thirty-five. Rather than build a control for every one, this saves the file the
 * bot actually reads — and the server parses it with the same loader the bot uses,
 * so a config that would break startup is refused before it lands on disk.
 */
export default function RawConfigEditor({
  bot,
  running,
  onSaved,
}: {
  bot: string
  running: boolean
  onSaved: (message: string) => void
}) {
  const [loaded, setLoaded] = useState<{
    toml: string
    path: string
    editable: boolean
  } | null>(null)
  const [draft, setDraft] = useState('')
  const [loadError, setLoadError] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  function load() {
    setLoadError(null)
    setLoaded(null)
    api
      .rawConfig(bot)
      .then((res) => {
        setLoaded(res)
        setDraft(res.toml)
      })
      .catch((e) => setLoadError(e instanceof ApiError ? e.message : String(e)))
  }

  useEffect(load, [bot])

  if (loadError) return <ErrorState error={loadError} onRetry={load} />
  if (!loaded) return <Loading what="the config file" />

  const dirty = draft !== loaded.toml

  async function save() {
    setBusy(true)
    setError(null)
    try {
      const res = await api.saveRawConfig(bot, draft)
      setLoaded({ ...loaded!, toml: draft })
      onSaved(res.message)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-3">
      <p className="text-xs text-faint">
        <code>{loaded.path}</code>
      </p>
      <TextArea
        value={draft}
        rows={24}
        spellCheck={false}
        disabled={!loaded.editable}
        onChange={(e) => setDraft(e.target.value)}
      />

      {error && <Banner tone="danger">{error}</Banner>}

      <div className="flex items-center gap-3">
        <Button
          variant="primary"
          busy={busy}
          disabled={!dirty || !loaded.editable}
          onClick={() => void save()}
        >
          {running ? 'Validate, save and restart' : 'Validate and save'}
        </Button>
        <Button disabled={!dirty} onClick={() => setDraft(loaded.toml)}>
          Revert
        </Button>
        <p className="text-xs text-faint">
          The panel parses this the way the bot does. If it wouldn't start on it,
          nothing is written.
          {!running && ' The bot is stopped and stays stopped.'}
        </p>
      </div>
    </div>
  )
}

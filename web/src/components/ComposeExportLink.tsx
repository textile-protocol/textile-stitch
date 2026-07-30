import { useState } from 'react'
import { ApiError, api } from '../api'

/**
 * Downloads the generated compose file.
 *
 * A button rather than an `<a href>` on purpose. A plain navigation to a protected
 * endpoint bypasses the API layer, so in password mode an expired session replaces
 * the whole SPA with the endpoint's 401 JSON and the operator has no way back to the
 * login form. Fetching it through `api` keeps the unauthorized handler in play, and
 * the file is built locally from the text.
 *
 * Styled to match whatever it replaced — a header link or an inline one — because
 * `className` comes from the caller.
 */
export default function ComposeExportLink({
  className,
  children,
  title,
}: {
  className?: string
  children: React.ReactNode
  title?: string
}) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  async function download() {
    setBusy(true)
    setError(null)
    try {
      save('docker-compose.yml', await api.composeExport())
    } catch (e) {
      // A 401 has already sent the operator to the login screen via the
      // unauthorized handler, so there is nothing useful to say here. Anything else
      // is worth showing next to the button they just pressed.
      setError(e instanceof ApiError && e.needsLogin ? null : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <>
      <button
        type="button"
        disabled={busy}
        onClick={() => void download()}
        className={className}
        title={title}
      >
        {children}
      </button>
      {error && <span className="text-xs text-danger">{error}</span>}
    </>
  )
}

/** Hand the browser a file without leaving the page. */
function save(name: string, text: string) {
  const url = URL.createObjectURL(new Blob([text], { type: 'application/yaml' }))
  const link = document.createElement('a')
  link.href = url
  link.download = name
  link.click()
  // The object URL pins the blob in memory until it's revoked, and the click has
  // already handed the data over by now.
  URL.revokeObjectURL(url)
}

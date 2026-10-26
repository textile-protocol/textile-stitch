import { useState } from 'react'
import { ApiError, api } from '../api'
import type { SessionInfo } from '../types'
import { Banner, Button, Field, Input } from './ui'

/** Legacy operators keep their fleet controls while they set a password. The
 * warning stays visible until the server confirms identity login is disabled. */
export default function PanelPasswordSetup({
  onSecured,
}: {
  onSecured: (session: SessionInfo) => void
}) {
  const [open, setOpen] = useState(false)
  const [password, setPassword] = useState('')
  const [confirmation, setConfirmation] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  function close() {
    setPassword('')
    setConfirmation('')
    setError(null)
    setOpen(false)
  }

  async function submit(event: React.FormEvent) {
    event.preventDefault()
    if (password !== confirmation) {
      setError('The passwords do not match.')
      return
    }
    setBusy(true)
    setError(null)
    try {
      const session = await api.setPanelPassword(password, confirmation)
      setPassword('')
      setConfirmation('')
      onSecured(session)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not save the password. Try again.')
    } finally {
      setBusy(false)
    }
  }

  return (
    <section
      className="mb-6 rounded-xl border border-warning/50 bg-surface p-5"
      aria-labelledby="panel-password-title"
    >
      <div role="alert" className="space-y-2">
        <h2 id="panel-password-title" className="text-base font-bold">
          Secure your panel with a password
        </h2>
        <p className="text-sm text-muted">
          This panel still accepts Tailscale identity alone. We recommend adding a
          password to protect access to your fleet and wallets. You can set it here;
          no terminal is needed, and your bots will keep running.
        </p>
      </div>
      {!open ? (
        <Button variant="primary" className="mt-4" onClick={() => setOpen(true)}>
          Set panel password
        </Button>
      ) : (
        <form
          method="post"
          action="/api/panel/password"
          onSubmit={submit}
          className="mt-4 max-w-md space-y-4"
        >
          <p className="text-sm text-muted">
            Choose a shared password for this panel and save it in your password
            manager. All panel operators will need it from now on. You will stay
            signed in; other sessions will be signed out.
          </p>
          <input
            type="text"
            name="username"
            autoComplete="username"
            value="stitch"
            readOnly
            tabIndex={-1}
            aria-hidden
            className="sr-only"
          />
          <Field label="New panel password">
            <Input
              name="password"
              type="password"
              autoComplete="new-password"
              autoFocus
              required
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              disabled={busy}
            />
          </Field>
          <p className="text-xs text-muted">Use at least 12 characters.</p>
          <Field label="Confirm panel password">
            <Input
              name="confirmation"
              type="password"
              autoComplete="new-password"
              required
              value={confirmation}
              onChange={(e) => setConfirmation(e.target.value)}
              disabled={busy}
            />
          </Field>
          {error && <Banner tone="danger">{error}</Banner>}
          <div className="flex gap-2">
            <Button type="submit" variant="primary" busy={busy}>Save password</Button>
            <Button type="button" disabled={busy} onClick={close}>Not now</Button>
          </div>
        </form>
      )}
    </section>
  )
}

import { useState } from 'react'
import { ApiError, api } from './../api'
import { Banner, Button, Card, Field, Input } from '../components/ui'
import type { SessionInfo } from '../types'

/**
 * The sign-in screen.
 *
 * Which form it shows depends on how the panel is configured, which the session
 * route tells us. A tailnet-only panel has no form at all — if the identity header
 * didn't arrive, no amount of typing here will help, so it explains that instead
 * of offering a box that can't work.
 */
export default function Login({
  session,
  onSignedIn,
}: {
  session: SessionInfo
  onSignedIn: () => void
}) {
  const [password, setPassword] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      await api.login(password)
      setPassword('')
      onSignedIn()
    } catch (err) {
      setError(err instanceof ApiError ? err.message : String(err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="grid min-h-screen place-items-center px-6">
      <div className="w-full max-w-sm space-y-4">
        <h1 className="text-center text-lg font-bold">
          Stitch
        </h1>

        {session.passwordLogin ? (
          <Card>
            <form
              onSubmit={submit}
              method="post"
              action="http://127.0.0.1:8420/api/login"
              className="space-y-4"
            >
              {/* Stable username so managers match the desktop signup form. */}
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
              <Field
                label="Panel password"
                hint="The password you chose in the Stitch desktop app (or STITCH_PANEL_PASSWORD_HASH)."
              >
                <Input
                  type="password"
                  name="password"
                  value={password}
                  autoFocus
                  autoComplete="current-password"
                  onChange={(e) => setPassword(e.target.value)}
                />
              </Field>
              {error && <Banner tone="danger">{error}</Banner>}
              <Button
                type="submit"
                variant="primary"
                busy={busy}
                disabled={password.length === 0}
                className="w-full justify-center"
              >
                Sign in
              </Button>
            </form>
          </Card>
        ) : (
          <Card>
            <Banner tone="warning">
              This panel authenticates by tailnet identity, and no identity reached
              it. Open it through its <code>tailscale serve</code> hostname rather
              than by IP or port-forward, and make sure your login is in
              STITCH_PANEL_TAILNET_USERS.
            </Banner>
          </Card>
        )}

        <p className="text-center text-xs text-faint">
          Whoever reaches this panel controls the Docker socket on this host.
        </p>
      </div>
    </div>
  )
}

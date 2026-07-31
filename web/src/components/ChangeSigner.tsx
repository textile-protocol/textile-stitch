import { useState } from 'react'
import { ApiError, api } from '../api'
import { Banner, Button, Card } from './ui'
import {
  SignerFields,
  buildSigner,
  emptySigner,
  isSignerComplete,
  type SignerState,
} from './SignerFields'

/**
 * Switch a bot's signer backend. Unlike the raw config editor, this collects the new
 * backend's credentials (which live outside the TOML) and the server writes them and
 * recreates the container with the matching runtime.
 */
export default function ChangeSigner({
  bot,
  onChanged,
}: {
  bot: string
  onChanged: (message: string) => void
}) {
  const [open, setOpen] = useState(false)
  const [signer, setSigner] = useState<SignerState>(emptySigner)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  function close() {
    setOpen(false)
    setSigner(emptySigner)
    setError(null)
  }

  async function submit() {
    setBusy(true)
    setError(null)
    try {
      const res = await api.changeSigner(bot, buildSigner(signer))
      // Clear the secret from component state the moment it's no longer needed.
      setSigner(emptySigner)
      setOpen(false)
      onChanged(res.message)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  if (!open) {
    return (
      <Card title="Signer">
        <p className="text-sm text-muted">
          Switch this bot's signer backend (hot wallet, Turnkey, or MPCVault). This
          writes the new credentials and recreates the container — a raw config edit
          can't, because the backend's secret lives outside the TOML.
        </p>
        <div className="mt-3">
          <Button onClick={() => setOpen(true)}>Change signer</Button>
        </div>
      </Card>
    )
  }

  return (
    <Card title="Change signer">
      <div className="space-y-4">
        <Banner tone="warning">
          This recreates {bot}'s container with the new backend. Orders it already
          signed stay on the book until they expire.
        </Banner>
        <SignerFields value={signer} onChange={setSigner} />
        {error && <Banner tone="danger">{error}</Banner>}
        <div className="flex justify-between">
          <Button onClick={close}>Cancel</Button>
          <Button
            variant="primary"
            busy={busy}
            disabled={!isSignerComplete(signer)}
            onClick={() => void submit()}
          >
            Switch and recreate
          </Button>
        </div>
      </div>
    </Card>
  )
}

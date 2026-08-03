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
import SignerConflictWarning from './SignerConflictWarning'

/**
 * Switch a bot's signer backend. Unlike the raw config editor, this collects the new
 * backend's credentials (which live outside the TOML) and the server writes them and
 * recreates the container with the matching runtime.
 */
export default function ChangeSigner({
  bot,
  chainId,
  wantsToBeUp,
  onChanged,
}: {
  bot: string
  /** Chain this bot trades on — wallet conflicts are per chain. */
  chainId: number | null | undefined
  /**
   * Whether changing the signer will restart this bot (running / restarting).
   * A live-transacting sibling then makes the server refuse the switch.
   */
  wantsToBeUp: boolean
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
    if (chainId != null) {
      try {
        const check = await api.checkSigner({
          chainId,
          signer: buildSigner(signer),
          excludeBot: bot,
        })
        if (check.conflicts.length > 0) {
          const names = check.conflicts.map((c) => c.name).join(', ')
          const blockers = check.conflicts
            .filter((c) => c.blocksLiveSwitch)
            .map((c) => c.name)
          // change_signer refuses a restart onto a wallet a live taker/closer
          // sibling already spends — don't offer "Switch anyway" for a 409.
          if (wantsToBeUp && blockers.length > 0) {
            setError(
              `${blockers.join(', ')} ${blockers.length === 1 ? 'is' : 'are'} live with taker/closer on and share this wallet. Stop ${blockers.length === 1 ? 'that bot' : 'those bots'} first — switching while both are up always fails.`,
            )
            return
          }
          if (
            !window.confirm(
              `Another bot already uses this wallet on chain ${chainId}: ${names}.\n\nSharing one wallet across bots on the same chain races nonces and will cause issues. Switch anyway?`,
            )
          ) {
            return
          }
        }
      } catch {
        // changeSigner will surface a bad key.
      }
    }
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
      <Card title="Wallet">
        <p className="text-sm text-muted">
          Create a new hot wallet, import an existing key, or switch to Turnkey /
          MPCVault. This writes the credentials and recreates the container — a
          raw config edit can't, because the backend's secret lives outside the
          TOML.
        </p>
        <div className="mt-3">
          <Button onClick={() => setOpen(true)}>Change wallet</Button>
        </div>
      </Card>
    )
  }

  return (
    <Card title="Change wallet">
      <div className="space-y-4">
        <Banner tone="warning">
          This recreates {bot}'s container with the new backend. Orders it already
          signed stay on the book until they expire.
        </Banner>
        <SignerFields value={signer} onChange={setSigner} />
        <SignerConflictWarning
          chainId={chainId}
          signer={signer}
          excludeBot={bot}
        />
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

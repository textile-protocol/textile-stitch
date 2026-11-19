// Hosted panels: Update and Rollback are one form that takes an exact image
// digest. Tags (including latest) are refused here and by the server, because
// a tag can move after review.

import { useState } from 'react'
import { ApiError, api } from '../api'
import { isDigestRef } from '../profile'
import type { Bot } from '../types'
import { Banner, Button, Field, Input } from './ui'

export default function DigestDeploy({
  bot,
  onDeployed,
}: {
  bot: Bot
  onDeployed: (message: string | null) => void
}) {
  const [image, setImage] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const valid = isDigestRef(image)
  const same = image.trim() === bot.image

  async function deploy() {
    if (!window.confirm(`Recreate ${bot.name} on ${image.trim()}? Its config stays as it is.`)) return
    setBusy(true)
    setError(null)
    try {
      const res = await api.deployDigest(bot.name, image.trim())
      setImage('')
      onDeployed(res.message)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-3">
      <p className="text-sm text-muted">
        Runs now: <span className="font-mono break-all">{bot.image ?? 'unknown'}</span>
      </p>
      <Field
        label="Image digest"
        hint="The reviewed release as repository@sha256:…, for an update or a rollback. Tags are refused."
      >
        <Input
          value={image}
          onChange={(e) => setImage(e.target.value)}
          placeholder="ghcr.io/textile-protocol/textile-stitch@sha256:…"
          spellCheck={false}
          autoComplete="off"
        />
      </Field>
      {image.trim() !== '' && !valid && (
        <p className="text-xs text-danger">Use repository@sha256: followed by 64 lowercase hex characters.</p>
      )}
      {error && <Banner tone="danger">{error}</Banner>}
      <Button variant="primary" busy={busy} disabled={!valid || same} onClick={() => void deploy()}>
        Deploy digest
      </Button>
    </div>
  )
}

// Re-register a bot with Textile. Enroll is idempotent for a bot that already
// has a credential (the venue hands the same maker back), so this is the safe
// recovery when the stream is stuck or a credential went missing. It lives
// under Tools because a seated bot never needs it; the state pill says when
// one might.

import { useState } from 'react'
import { ApiError, api } from '../api'
import { Banner, Button } from './ui'

export default function ReconnectTextile({ bot, onDone }: { bot: string; onDone?: () => void }) {
  const [busy, setBusy] = useState(false)
  const [note, setNote] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  async function reconnect() {
    setBusy(true)
    setError(null)
    setNote(null)
    try {
      const res = await api.enrollRfq(bot)
      setNote(res.message)
      onDone?.()
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-3">
      <p className="text-sm text-muted">
        Re-registers this bot with Textile. Only for a connection that is stuck; a bot
        that shows <strong>live</strong> does not need it.
      </p>
      {note && <Banner tone="info">{note}</Banner>}
      {error && <Banner tone="danger">{error}</Banner>}
      <Button busy={busy} onClick={() => void reconnect()}>
        Reconnect to Textile
      </Button>
    </div>
  )
}

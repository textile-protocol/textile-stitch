import { useEffect, useState } from 'react'

const DATAROOM_ORIGIN = 'https://dataroom.textilecredit.com'
const DATAROOM_STITCH_DASHBOARD = `${DATAROOM_ORIGIN}/stitch-dashboard/`
/** Fallback before the first embed-height postMessage lands. */
const INITIAL_IFRAME_HEIGHT_PX = 480
const EMBED_HEIGHT_SOURCE = 'textile-stitch-dashboard'
const EMBED_HEIGHT_TYPE = 'embed-height'

function dashboardUrl(wallet: string, botName?: string): string {
  const url = new URL(DATAROOM_STITCH_DASHBOARD)
  url.searchParams.set('bot', wallet)
  url.searchParams.set('embed', '1')
  // Browsers often skip navigating an iframe when only the query changes.
  // The panel bot name makes the URL unique per switch so the frame reloads.
  if (botName) url.searchParams.set('panel', botName)
  return url.toString()
}

function isEmbedHeightMessage(
  data: unknown,
): data is { source: string; type: string; height: number } {
  if (!data || typeof data !== 'object') return false
  const msg = data as { source?: unknown; type?: unknown; height?: unknown }
  return (
    msg.source === EMBED_HEIGHT_SOURCE &&
    msg.type === EMBED_HEIGHT_TYPE &&
    typeof msg.height === 'number' &&
    Number.isFinite(msg.height) &&
    msg.height > 0
  )
}

/**
 * Production stitch dashboard for one maker wallet, framed without dataroom
 * chrome (`?embed=1` strips nav + header + bot picker on the remote page).
 * Height follows postMessage from the embed so the panel scrolls, not the iframe.
 *
 * `wallet` is whatever the chain sees trading — the OperatorVault for a vault
 * maker, the bot's own wallet otherwise. The dashboard's maker index is keyed by
 * that address, so passing the signing key for a vault bot finds nothing.
 */
export default function StitchDashboardEmbed({
  wallet,
  botName,
}: {
  wallet: string | null | undefined
  botName?: string
}) {
  const [heightPx, setHeightPx] = useState(INITIAL_IFRAME_HEIGHT_PX)
  const frameKey = `${botName ?? ''}:${wallet ?? ''}`

  useEffect(() => {
    if (!wallet) return
    setHeightPx(INITIAL_IFRAME_HEIGHT_PX)

    function onMessage(event: MessageEvent) {
      if (event.origin !== DATAROOM_ORIGIN) return
      if (!isEmbedHeightMessage(event.data)) return
      const next = Math.ceil(event.data.height)
      setHeightPx((prev) => (prev === next ? prev : next))
    }

    window.addEventListener('message', onMessage)
    return () => window.removeEventListener('message', onMessage)
  }, [wallet, botName])

  if (!wallet) {
    return (
      <p className="text-sm text-muted">
        No maker wallet on this bot&apos;s config. Dashboard stats need either a
        vault address or the operator address from settings.
      </p>
    )
  }

  return (
    <section className="overflow-hidden rounded-xl border border-line-soft bg-[#22242a]">
      <iframe
        key={frameKey}
        title="Stitch dashboard"
        src={dashboardUrl(wallet, botName)}
        className="block w-full border-0"
        style={{ height: heightPx, overflow: 'hidden' }}
      />
    </section>
  )
}

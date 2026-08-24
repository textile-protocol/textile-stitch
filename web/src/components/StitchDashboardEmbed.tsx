import { useEffect, useState } from 'react'

const DATAROOM_ORIGIN = 'https://dataroom.textilecredit.com'
const DATAROOM_STITCH_DASHBOARD = `${DATAROOM_ORIGIN}/stitch-dashboard/`
/** Fallback before the first embed-height postMessage lands. */
const INITIAL_IFRAME_HEIGHT_PX = 480
const EMBED_HEIGHT_SOURCE = 'textile-stitch-dashboard'
const EMBED_HEIGHT_TYPE = 'embed-height'

function dashboardUrl(operatorAddress: string, botName?: string): string {
  const url = new URL(DATAROOM_STITCH_DASHBOARD)
  url.searchParams.set('bot', operatorAddress)
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
 * Production stitch dashboard for one operator wallet, framed without dataroom
 * chrome (`?embed=1` strips nav + header + bot picker on the remote page).
 * Height follows postMessage from the embed so the panel scrolls, not the iframe.
 */
export default function StitchDashboardEmbed({
  operatorAddress,
  botName,
}: {
  operatorAddress: string | null | undefined
  botName?: string
}) {
  const [heightPx, setHeightPx] = useState(INITIAL_IFRAME_HEIGHT_PX)
  const frameKey = `${botName ?? ''}:${operatorAddress ?? ''}`

  useEffect(() => {
    if (!operatorAddress) return
    setHeightPx(INITIAL_IFRAME_HEIGHT_PX)

    function onMessage(event: MessageEvent) {
      if (event.origin !== DATAROOM_ORIGIN) return
      if (!isEmbedHeightMessage(event.data)) return
      const next = Math.ceil(event.data.height)
      setHeightPx((prev) => (prev === next ? prev : next))
    }

    window.addEventListener('message', onMessage)
    return () => window.removeEventListener('message', onMessage)
  }, [operatorAddress, botName])

  if (!operatorAddress) {
    return (
      <p className="text-sm text-muted">
        No operator wallet on this bot&apos;s config. Dashboard stats need the
        operator address from settings.
      </p>
    )
  }

  return (
    <section className="overflow-hidden rounded-xl border border-line-soft bg-[#22242a]">
      <iframe
        key={frameKey}
        title="Stitch dashboard"
        src={dashboardUrl(operatorAddress, botName)}
        className="block w-full border-0"
        style={{ height: heightPx, overflow: 'hidden' }}
      />
    </section>
  )
}

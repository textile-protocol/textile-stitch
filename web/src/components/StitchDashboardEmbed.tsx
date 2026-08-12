const DATAROOM_STITCH_DASHBOARD =
  'https://dataroom.textilecredit.com/stitch-dashboard/'

function dashboardUrl(operatorAddress: string): string {
  const url = new URL(DATAROOM_STITCH_DASHBOARD)
  url.searchParams.set('bot', operatorAddress)
  url.searchParams.set('embed', '1')
  return url.toString()
}

/**
 * Production stitch dashboard for one operator wallet, framed without dataroom
 * chrome (`?embed=1` strips nav + header + bot picker on the remote page).
 */
export default function StitchDashboardEmbed({
  operatorAddress,
}: {
  operatorAddress: string | null | undefined
}) {
  if (!operatorAddress) {
    return (
      <p className="text-sm text-muted">
        No operator wallet on this bot&apos;s config. Dashboard stats need the
        operator address from settings.
      </p>
    )
  }

  return (
    <iframe
      title="Stitch dashboard"
      src={dashboardUrl(operatorAddress)}
      className="block w-full border-0 bg-[#22242a]"
      style={{ minHeight: '80vh', height: '80vh' }}
    />
  )
}

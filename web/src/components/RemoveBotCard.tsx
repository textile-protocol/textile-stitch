// The Remove/Delete card at the bottom of Tools. Removing deletes the key, so
// a hosted panel refuses it server-side and this card doesn't render there.

import { useAllows } from '../profile'
import type { Funding } from '../types'
import { Banner, Button, Card } from './ui'

export default function RemoveBotCard(props: {
  funding: Funding | null
  hasContainer: boolean
  busy: boolean
  onRemove: () => void
}) {
  if (!useAllows('removeBot')) return null
  return (
    <Card title={props.hasContainer ? 'Remove this bot' : 'Delete this bot'}>
      <RemoveBot {...props} />
    </Card>
  )
}

/**
 * The Remove button and the reason it may be disabled. The server decides
 * (`removeBlockedBy` comes with every wallet read, and the remove route
 * applies the same rule again before it deletes the key); this only shows the
 * answer before the click. While the wallet has not been read yet the button
 * stays live: a bot whose config the panel cannot read is one the route
 * deletes nothing for, and an unreadable wallet is refused by the route.
 */
function RemoveBot({
  funding,
  hasContainer,
  busy,
  onRemove,
}: {
  funding: Funding | null
  hasContainer: boolean
  busy: boolean
  onRemove: () => void
}) {
  const blocked = funding?.removeBlockedBy ?? null
  return (
    <div className="space-y-3">
      <p className="text-sm text-muted">
        {hasContainer
          ? 'Deletes the container, the config and the private key. Gone from the fleet, and cannot be undone.'
          : 'Deletes the config and the private key. Gone from the fleet, and cannot be undone.'}
      </p>
      {blocked && <Banner tone="warning">{blocked}</Banner>}
      <Button variant="danger" busy={busy} disabled={blocked !== null} onClick={onRemove}>
        {hasContainer ? 'Remove' : 'Delete'}
      </Button>
    </div>
  )
}

// Shared Remove confirms for Fleet and bot detail.
//
// Cancel always aborts. The old detail flow used Cancel on a second dialog to
// mean "keep the config", which still destroyed the container and left a
// config-only zombie on the fleet — looking like Remove did nothing.

/** What the operator picked before we hit the API. `null` means abort. */
export type RemovePlan = { deleteConfig: boolean } | null

/**
 * Ask what to remove. Always deletes config (and the container when there is
 * one). Cancel aborts — nothing is removed.
 */
export function confirmRemovePlan(opts: {
  name: string
  hasContainer: boolean
}): RemovePlan {
  const { name, hasContainer } = opts
  if (hasContainer) {
    if (
      !window.confirm(
        `Remove ${name} from the fleet?\n\nThis deletes the container, config, and private key. Cannot be undone.\n\nOK removes it. Cancel aborts — nothing is removed.`,
      )
    ) {
      return null
    }
    return { deleteConfig: true }
  }

  if (
    !window.confirm(
      `Delete ${name}'s config and private key?\n\nThere is no container. This removes it from the fleet and cannot be undone.`,
    )
  ) {
    return null
  }
  return { deleteConfig: true }
}

// Shared Remove confirms for Fleet and bot detail.
//
// Cancel always aborts. The old detail flow used Cancel on a second dialog to
// mean "keep the config", which still destroyed the container and left a
// config-only zombie on the fleet — looking like Remove did nothing.

/** What the operator picked before we hit the API. `null` means abort. */
export type RemovePlan = { deleteConfig: boolean } | null

/**
 * Ask what to remove.
 *
 * - No container: one confirm, always deletes config (the only thing left).
 * - Has container: one confirm for a full delete (container + config + key).
 *   Detail pages can also offer {@link confirmRemoveContainerOnlyPlan}.
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

/**
 * Detail-only escape hatch: destroy the container, keep files on disk.
 *
 * The bot reappears on the fleet as config-only so Recreate can bring it back.
 * Explicit about that, because Cancel-means-keep on the main Remove path is how
 * operators ended up with zombies they couldn't see how to delete.
 */
export function confirmRemoveContainerOnlyPlan(name: string): RemovePlan {
  if (
    !window.confirm(
      `Remove ${name}'s container but keep its config and private key?\n\n${name} will stay on the fleet as config-only until you Delete it (or Recreate the container).`,
    )
  ) {
    return null
  }
  return { deleteConfig: false }
}

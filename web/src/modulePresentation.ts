import {
  moduleStatusFresh,
  type ModulesConfig,
  type ModulesView,
} from './modules'

export type ModuleKey = 'inventory' | 'spreads' | 'rebalance'
export const moduleNames: Record<ModuleKey, string> = {
  inventory: 'Inventory balancing',
  spreads: 'Dynamic spreads',
  rebalance: 'Spot rebalancing',
}
export const moduleKeys: ModuleKey[] = ['inventory', 'spreads', 'rebalance']
export const percentage = (bps: number) =>
  `${(bps / 100).toLocaleString(undefined, { maximumFractionDigits: 2 })}%`

/** UI validation mirrors the numeric config constraints. The server remains
 * authoritative, including dealer URL and wallet validation. Never repair a
 * related setting silently when a slider moves. */
export function moduleParameterErrors(c: ModulesConfig): string[] {
  const integer = (n: number, min: number, max: number) =>
    Number.isInteger(n) && n >= min && n <= max
  const i = c.inventory,
    s = c.spreads,
    r = c.rebalance
  return [
    (!integer(i.target_bps, 1, 9998) ||
      !integer(i.max_bps, 2, 9999) ||
      i.target_bps >= i.max_bps) &&
      'Inventory: target must be above 0% and below the purchase limit, which must be below 100%.',
    (!integer(i.max_skew_bps, 0, 4999) ||
      !integer(i.spread_floor_bps, 0, 4999)) &&
      'Inventory advanced settings: price adjustment and minimum margin must be 0–49.99%, in steps of 0.01%.',
    (!integer(s.window_secs, 10, 3600) ||
      !integer(s.warmup_secs, 1, 3599) ||
      s.warmup_secs >= s.window_secs) &&
      'Dynamic spreads timing: use a 10–3,600 second window and a shorter, positive warmup.',
    (!Number.isFinite(s.multiplier) ||
      s.multiplier < 0 ||
      s.multiplier > 10 ||
      !integer(s.max_extra_bps, 0, 4999)) &&
      'Dynamic spreads: sensitivity must be 0–10× and the extra margin 0–49.99%, in steps of 0.01%.',
    r.enabled &&
      (!integer(r.trigger_bps, 1, 9999) ||
        r.trigger_bps <= i.target_bps ||
        r.trigger_bps > i.max_bps) &&
      'Spot rebalancing: the sale threshold must be above the inventory target and at or below the purchase limit.',
    (!integer(r.max_trade_bps, 1, 1000) ||
      !integer(r.max_slippage_bps, 0, 999)) &&
      'Spot rebalancing: each sale must be 0.01–10% of vault value and the price discount 0–9.99%, in steps of 0.01%.',
    (!integer(r.order_lifetime_secs, 10, 300) ||
      !Number.isSafeInteger(r.cooldown_secs) ||
      r.cooldown_secs < r.order_lifetime_secs + 30) &&
      'Spot timing: orders last 10–300 seconds; the wait between sales must be at least 30 seconds longer.',
  ].filter((message): message is string => typeof message === 'string')
}

/** Illustrations only: the same integer rounding as the Rust strategies, with
 * explicit example inputs. Excludes reservations, data freshness and execution. */
export function inventoryIllustration(
  c: ModulesConfig,
  share: number,
  base = 20
) {
  const i = c.inventory
  if (!i.enabled) return { buy: base, sell: base }
  const skew = Math.min(
    i.max_skew_bps,
    Math.floor(
      (i.max_skew_bps * Math.abs(share - i.target_bps)) /
        (i.max_bps - i.target_bps)
    )
  )
  return share >= i.target_bps
    ? {
        buy:
          share >= i.max_bps
            ? null
            : Math.min(9999, Math.max(i.spread_floor_bps, base + skew)),
        sell: Math.max(i.spread_floor_bps, base - skew),
      }
    : {
        buy: Math.max(i.spread_floor_bps, base - skew),
        sell: Math.max(i.spread_floor_bps, base + skew),
      }
}
export function volatilityIllustration(
  c: ModulesConfig,
  share: number,
  rangeBps: number
) {
  const extra = c.spreads.enabled
    ? Math.min(
        c.spreads.max_extra_bps,
        Math.ceil(rangeBps * c.spreads.multiplier)
      )
    : 0
  const buyPaused = c.inventory.enabled && share >= c.inventory.max_bps
  if (!c.spreads.inventory_aware || !c.inventory.enabled)
    return { buy: buyPaused ? null : extra, sell: extra }
  const above = share >= c.inventory.target_bps
  const band = above
    ? c.inventory.max_bps - c.inventory.target_bps
    : c.inventory.target_bps
  const discount = Math.floor(
    (extra * Math.min(Math.abs(share - c.inventory.target_bps), band)) / band
  )
  return {
    buy: share >= c.inventory.max_bps ? null : above ? extra : extra - discount,
    sell: above ? extra - discount : extra,
  }
}

export function moduleOverviewState(view: ModulesView, now: number) {
  const fresh = moduleStatusFresh(view.status, view.running, now)
  const c = view.status?.config ?? view.config
  const d = view.status?.decisions.at(-1)?.decision
  if (!view.running)
    return {
      title: 'Ready when you are',
      message:
        'The bot is stopped. You can edit settings or try a historical simulation.',
      current: false,
    }
  if (
    c.mode === 'off' &&
    view.status &&
    view.status.at <= now &&
    now - view.status.at <= 10
  )
    return {
      title: 'Modules are off',
      message: 'The bot uses its regular quote settings.',
      current: false,
    }
  if (!fresh)
    return {
      title: 'Waiting for an update',
      message:
        'Current values will appear when the bot has a connection, prices and vault data.',
      current: false,
    }
  if (d?.blocked)
    return {
      title: d.reasons.some((r) => r.includes('Collecting price history'))
        ? 'Getting ready'
        : c.mode === 'shadow'
          ? 'Preview is waiting'
          : 'Quotes are paused',
      message: d.reasons.some((r) => r.includes('Collecting price history'))
        ? 'Collecting enough price history to calculate spreads.'
        : 'The latest evaluation could not produce quotes. Open details for the reason.',
      current: true,
    }
  if (c.mode === 'shadow')
    return {
      title: 'Previewing your strategy',
      message:
        'These are proposed adjustments. Your live quotes stay unchanged.',
      current: true,
    }
  if (!moduleKeys.some((key) => c[key].enabled))
    return {
      title: 'No modules enabled',
      message: 'Choose a module in Parameters to adjust the bot’s behavior.',
      current: true,
    }
  return {
    title: 'Your modules are running',
    message: 'Quote adjustments follow the bot’s active settings.',
    current: true,
  }
}

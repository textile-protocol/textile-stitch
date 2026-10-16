export interface ModulesConfig {
  mode: 'off' | 'shadow' | 'live'
  inventory: {
    enabled: boolean
    target_bps: number
    max_bps: number
    max_skew_bps: number
    spread_floor_bps: number
  }
  spreads: {
    enabled: boolean
    /** Absent in status/config responses from older releases; defaults off. */
    inventory_aware?: boolean
    window_secs: number
    warmup_secs: number
    multiplier: number
    max_extra_bps: number
  }
  rebalance: {
    method?: 'manual' | 'dealer'
    enabled: boolean
    trigger_bps: number
    max_trade_bps: number
    max_slippage_bps: number
    cooldown_secs: number
    order_lifetime_secs: number
    dealer: { url: string; taker: string; api_key_env: string | null } | null
  }
}
export interface ModuleDecision {
  version: number
  at: number
  inventory_bps: number | null
  buy_bps: number | null
  sell_bps: number | null
  buy_limit: string
  rebalance_sell: string
  volatility_bps: number
  reasons: string[]
  blocked: boolean
}
export interface ModuleStatus {
  version: number
  config: ModulesConfig
  at: number
  decisions: {
    decision: ModuleDecision
    price: number
    settlement: string
    corridor: string
    inputs?: {
      price_at: number
      balances_at: number
      base_buy_bps: number | null
      base_sell_bps: number | null
      inventory_buy_bps: number | null
      inventory_sell_bps: number | null
      spread_window: {
        samples: number
        history_secs: number
        low: number | null
        high: number | null
        extra_bps: number | null
      }
    } | null
  }[]
  rebalance_status: string
  next_attempt_at: number
  quote_status?: {
    at: number
    state:
      | 'waiting_for_session'
      | 'waiting_for_vault'
      | 'waiting_for_price'
      | 'stale_price'
      | 'no_corridor'
      | 'evaluating'
    message: string
  } | null
}
/** One side of the corridor's configured spread. `abs` is an offset in
 * collateral per debt, so it only reads as bps at a price. */
export interface CorridorSpread {
  kind: 'bps' | 'abs'
  value: number
}
export interface ModulesView {
  config: ModulesConfig
  revision: string
  status: ModuleStatus | null
  running: boolean
  settlement_decimals: number
  corridor_decimals: number
  /** What the modules start from. Null is a side the corridor leaves off. */
  corridor_spreads: { buy: CorridorSpread | null; sell: CorridorSpread | null }
}

/** Recorded changes, including side vetoes and caps; never recalculate from
 * today's configuration or assume the common volatility buffer was applied. */
export function dynamicSpreadAdditions(
  observation: ModuleStatus['decisions'][number] | undefined
): { buy: number | null | undefined; sell: number | null | undefined } {
  if (!observation?.inputs || observation.decision.blocked)
    return { buy: undefined, sell: undefined }
  const { decision, inputs } = observation
  const delta = (before: number | null, after: number | null) =>
    after === null ? null : before === null ? undefined : after - before
  return {
    buy: delta(inputs.inventory_buy_bps, decision.buy_bps),
    sell: delta(inputs.inventory_sell_bps, decision.sell_bps),
  }
}
export function moduleStatusFresh(
  status: ModuleStatus | null,
  running: boolean,
  now: number
): boolean {
  const at = status?.decisions.at(-1)?.decision.at
  return (
    !!status &&
    running &&
    at !== undefined &&
    at <= now &&
    now - at <= 10 &&
    status.at <= now &&
    now - status.at <= 10 &&
    (!status.quote_status ||
      (status.quote_status.state === 'evaluating' &&
        status.quote_status.at <= now &&
        now - status.quote_status.at <= 10))
  )
}

/**
 * Dynamic spreads have not seen enough price history yet. Current bots keep
 * quoting at the maximum addition and lead with "Warming up dynamic spreads";
 * older bots blocked both sides with "Collecting price history".
 */
export function isWarmingUp(decision: { reasons: string[] }): boolean {
  return decision.reasons.some(
    (r) =>
      r.startsWith('Warming up dynamic spreads') ||
      r.includes('Collecting price history')
  )
}

export function dynamicSpreadState(
  view: ModulesView,
  now: number
): { label: string; message: string; current: boolean } {
  const status = view.status
  const last = status?.decisions.at(-1)
  if (!view.running)
    return {
      label: 'Bot stopped',
      message:
        'Start the bot to observe dynamic spreads. Saved Live mode alone does not start it.',
      current: false,
    }
  if (!status)
    return {
      label: 'No runtime data',
      message:
        'The panel has not received a module status file. Check that the bot supports modules, was restarted after enabling them, and shares its run directory with the panel.',
      current: false,
    }
  if (status.quote_status && status.quote_status.state !== 'evaluating')
    return {
      label: 'Waiting for data or connection',
      message: status.quote_status.message,
      current: false,
    }
  if (!moduleStatusFresh(status, view.running, now))
    return {
      label: 'Telemetry stale',
      message:
        'No current module evaluation. Check the RFQ connection, price feed and vault RPC reads in the bot logs. Previous readings are retained below.',
      current: false,
    }
  if (!status.config.spreads.enabled || status.config.mode === 'off')
    return {
      label: 'Dynamic spreads off',
      message:
        'The running configuration does not apply dynamic spreads. Check the saved settings and restart status.',
      current: false,
    }
  if (last?.decision.blocked)
    return {
      label: isWarmingUp(last.decision) ? 'Warming up' : 'Quote policy blocked',
      message: last.decision.reasons.join('. '),
      current: true,
    }
  if (last && isWarmingUp(last.decision))
    return {
      label: 'Warming up · maximum buffer',
      message: `Collecting ${status.config.spreads.warmup_secs}s of price history. Until then quotes stay live and carry the maximum addition of ${status.config.spreads.max_extra_bps} bps.`,
      current: true,
    }
  const shadow = status.config.mode === 'shadow'
  const additions = dynamicSpreadAdditions(last)
  const inventoryRemovesBuffer =
    status.config.spreads.inventory_aware &&
    status.config.inventory.enabled &&
    !!last?.decision.volatility_bps &&
    (last.decision.inventory_bps === 0 ||
      (last.decision.inventory_bps ?? 0) >= status.config.inventory.max_bps) &&
    (additions.buy === 0 || additions.sell === 0) &&
    (additions.buy === 0 || additions.buy === null) &&
    (additions.sell === 0 || additions.sell === null)
  return {
    label: shadow ? 'Shadow · preview only' : 'Live quote policy',
    current: true,
    message: shadow
      ? 'These adjustments are recorded for comparison. Shadow mode leaves live quotes unchanged.'
      : inventoryRemovesBuffer
        ? 'Inventory weighting removes the extra volatility buffer on the enabled side at this exposure. Inventory-adjusted spreads and side limits still apply.'
        : last?.decision.volatility_bps === 0
          ? status.config.spreads.multiplier === 0 ||
            status.config.spreads.max_extra_bps === 0
            ? 'The running multiplier or maximum addition is zero, so dynamic spreads add nothing.'
            : 'The observed prices are unchanged within the rolling window. The module is active and adds 0 bps.'
          : 'The dynamic addition is included in quote calculations. Available inventory, vault controls and venue checks still determine whether a quote is offered.',
  }
}
export function downloadJson(name: string, data: unknown) {
  const url = URL.createObjectURL(
    new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' })
  )
  const a = document.createElement('a')
  a.href = url
  a.download = name
  a.click()
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}

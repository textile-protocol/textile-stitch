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
export interface ModulesView {
  config: ModulesConfig
  revision: string
  status: ModuleStatus | null
  running: boolean
  settlement_decimals: number
  dataset_template: Record<string, unknown>
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
export interface SimulationMetrics {
  ending_nav: string
  pnl: string
  return_pct: number
  max_drawdown_bps: number
  customer_fills: number
  rebalance_fills: number
  execution_costs: string
  max_inventory_bps: number
}
export interface SimulationReport {
  version: number
  config: ModulesConfig
  baseline: SimulationMetrics
  candidate: SimulationMetrics
  equity: { at: number; baseline: string; candidate: string }[]
  assumptions: string[]
  events: number
  dealer_observations: number
}
export interface HistoryOptions {
  from: number
  to: number
  cost_per_trade: string
  dealer_scenario: null | {
    slippage_bps: number
    max_corridor_per_observation: string
  }
}
export interface HistoricalSimulation {
  report: SimulationReport
  dataset: unknown
  options: HistoryOptions
  source: {
    vault: string
    chain_id: number
    from: number
    to: number
    collected_at: number
    price_source: string
    indexed_through: number
    config_revision: string
    staleness_secs: number
    snapshot: { block_number: string; block_hash: string; at: number }
  }
  coverage: {
    observed_trades: number
    replayed_trades: number
    skipped_stale_trades: number
    price_observations: number
    fresh_seconds: number
    total_seconds: number
    max_price_gap_secs: number
  }
  analysis: {
    return_delta_pp: number
    drawdown_delta_bps: number
    inventory_delta_bps: number
    fill_delta: number
    conclusion: string
  }
}

/** Decimal user input to exact atomic units, without Number rounding. */
export function simulationAmount(value: string, decimals: number): string {
  if (
    !Number.isInteger(decimals) ||
    decimals < 0 ||
    decimals > 18 ||
    !/^\d+(\.\d+)?$/.test(value)
  ) {
    throw new Error('Enter a non-negative amount using a decimal point.')
  }
  const [whole, fraction = ''] = value.split('.')
  if (fraction.length > decimals)
    throw new Error(`Use at most ${decimals} decimal places.`)
  const atomic =
    BigInt(whole!) * 10n ** BigInt(decimals) +
    BigInt(fraction.padEnd(decimals, '0') || '0')
  if (atomic >= 2n ** 256n) throw new Error('Amount is too large.')
  return atomic.toString()
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
      label: last.decision.reasons.some((r) =>
        r.includes('Collecting price history')
      )
        ? 'Warming up'
        : 'Quote policy blocked',
      message: last.decision.reasons.join('. '),
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
export function equityCoordinates(
  points: SimulationReport['equity'],
  key: 'baseline' | 'candidate'
): string {
  if (!points.length) return ''
  const values = points.flatMap((p) => [
    BigInt(p.baseline),
    BigInt(p.candidate),
  ])
  const low = values.reduce((a, b) => (a < b ? a : b))
  const high = values.reduce((a, b) => (a > b ? a : b))
  const span = high - low || 1n
  const first = points[0]?.at ?? 0
  const elapsed = (points.at(-1)?.at ?? first) - first || 1
  return points
    .map(
      (p) =>
        `${20 + ((p.at - first) * 660) / elapsed},${180 - Number(((BigInt(p[key]) - low) * 150n) / span)}`
    )
    .join(' ')
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

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
  }[]
  rebalance_status: string
  next_attempt_at: number
}
export interface ModulesView {
  config: ModulesConfig
  revision: string
  status: ModuleStatus | null
  running: boolean
  settlement_decimals: number
  dataset_template: Record<string, unknown>
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
    now - status.at <= 10
  )
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

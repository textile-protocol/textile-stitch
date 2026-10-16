import {
  isWarmingUp,
  moduleStatusFresh,
  type CorridorSpread,
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

type Mode = ModulesConfig['mode']
/** `shadow` is called Preview in the panel. `effect` says what saving does. */
export const moduleModes: readonly {
  value: Mode
  label: string
  hint: string
  effect: string
}[] = [
  {
    value: 'off',
    label: 'Off',
    hint: 'Regular quotes',
    effect:
      'Saving turns off new module activity. Already signed orders remain valid until they expire.',
  },
  {
    value: 'shadow',
    label: 'Preview',
    hint: 'Observe only',
    effect:
      'Preview records proposed adjustments without changing live quotes.',
  },
  {
    value: 'live',
    label: 'Live',
    hint: 'Apply to quotes',
    effect:
      'Saving applies these settings to live quotes and restarts a running bot.',
  },
]

// A typed-over field can be NaN; show a dash rather than "NaN%".
const shownPercent = (v: unknown) =>
  typeof v === 'number' && Number.isFinite(v) ? percentage(v) : '—'
const shownSeconds = (v: unknown) =>
  typeof v === 'number' && Number.isFinite(v)
    ? `${v.toLocaleString()} sec`
    : '—'
const shownSwitch = (v: unknown) => (v ? 'On' : 'Off')

/** The two settings that define each module, shown on its strategy row. */
export function moduleKeyFigures(
  c: ModulesConfig,
  key: ModuleKey
): { label: string; value: string }[] {
  const figures: Record<ModuleKey, [string, string][]> = {
    inventory: [
      ['Target', shownPercent(c.inventory.target_bps)],
      ['Pause at', shownPercent(c.inventory.max_bps)],
    ],
    spreads: [
      ['Max extra', `+${shownPercent(c.spreads.max_extra_bps)}`],
      [
        'Sensitivity',
        Number.isFinite(c.spreads.multiplier)
          ? `${c.spreads.multiplier}×`
          : '—',
      ],
    ],
    rebalance: [
      ['Sell above', shownPercent(c.rebalance.trigger_bps)],
      ['Per sale', shownPercent(c.rebalance.max_trade_bps)],
    ],
  }
  return figures[key].map(([label, value]) => ({ label, value }))
}

/** Every setting the Parameters form edits, labeled as the form labels it.
 * Dealer credentials are TOML-only and not listed. */
const editableSettings: readonly [
  string,
  (c: ModulesConfig) => unknown,
  (v: unknown) => string,
][] = [
  [
    'Run mode',
    (c) => c.mode,
    (v) => moduleModes.find((m) => m.value === v)?.label ?? String(v),
  ],
  ['Inventory balancing', (c) => c.inventory.enabled, shownSwitch],
  ['Target holdings', (c) => c.inventory.target_bps, shownPercent],
  ['Pause purchases at', (c) => c.inventory.max_bps, shownPercent],
  ['Maximum price adjustment', (c) => c.inventory.max_skew_bps, shownPercent],
  ['Minimum quote margin', (c) => c.inventory.spread_floor_bps, shownPercent],
  ['Dynamic spreads', (c) => c.spreads.enabled, shownSwitch],
  ['Maximum extra margin', (c) => c.spreads.max_extra_bps, shownPercent],
  [
    'Sensitivity',
    (c) => c.spreads.multiplier,
    (v) => (Number.isFinite(v) ? `${v}×` : '—'),
  ],
  [
    'Favor inventory reduction',
    (c) => c.spreads.inventory_aware ?? false,
    shownSwitch,
  ],
  ['Price history window', (c) => c.spreads.window_secs, shownSeconds],
  ['Warmup time', (c) => c.spreads.warmup_secs, shownSeconds],
  ['Spot rebalancing', (c) => c.rebalance.enabled, shownSwitch],
  [
    'Sale method',
    (c) => c.rebalance.method ?? 'manual',
    (v) => (v === 'dealer' ? 'Dealer API' : 'Manual'),
  ],
  ['Suggest a sale above', (c) => c.rebalance.trigger_bps, shownPercent],
  ['Maximum per sale', (c) => c.rebalance.max_trade_bps, shownPercent],
  ['Maximum price discount', (c) => c.rebalance.max_slippage_bps, shownPercent],
  ['Wait between sales', (c) => c.rebalance.cooldown_secs, shownSeconds],
  ['Order lifetime', (c) => c.rebalance.order_lifetime_secs, shownSeconds],
]

/** What saving would change, in form order. `Object.is` so an empty field
 * (NaN) counts as one change, not a change on every render. */
export const moduleChanges = (
  saved: ModulesConfig,
  draft: ModulesConfig
): { label: string; from: string; to: string }[] =>
  editableSettings
    .filter(([, read]) => !Object.is(read(saved), read(draft)))
    .map(([label, read, show]) => ({
      label,
      from: show(read(saved)),
      to: show(read(draft)),
    }))

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

/** A buy and a sell spread in whole bps. Null is a side that doesn't quote. */
export type QuoteSides = { buy: number | null; sell: number | null }

/** One corridor side in whole bps, read the way `modules::base_spreads` reads
 * it: bps as set, an absolute offset converted at `price` (settlement per
 * corridor token) and rounded up. Null for a side the corridor leaves off;
 * undefined for an absolute offset with no usable price. */
export function corridorSpreadBps(
  spread: CorridorSpread | null,
  side: 'buy' | 'sell',
  price: number | undefined
): number | null | undefined {
  if (spread === null) return null
  if (spread.kind === 'bps') return spread.value
  if (price === undefined || !Number.isFinite(price) || price <= 0)
    return undefined
  // The offset moves collateral per debt away from the mid on each side.
  const perDebt =
    side === 'buy' ? 1 / price + spread.value : 1 / price - spread.value
  const quoted = perDebt > 0 ? 1 / perDebt : 0
  const bps =
    side === 'buy' ? (1 - quoted / price) * 10000 : (quoted / price - 1) * 10000
  return Math.ceil(Math.max(0, bps))
}

/** Both corridor sides, or null when either can't be read in bps yet. */
export function corridorSpreads(
  spreads: ModulesView['corridor_spreads'],
  price: number | undefined
): QuoteSides | null {
  const buy = corridorSpreadBps(spreads.buy, 'buy', price)
  const sell = corridorSpreadBps(spreads.sell, 'sell', price)
  return buy === undefined || sell === undefined ? null : { buy, sell }
}

/** Illustrations only: the same integer rounding as the Rust strategies, with
 * explicit example inputs. Excludes reservations, data freshness and execution.
 * Inventory balancing moves the corridor's `base` spreads; off returns them. */
export function inventoryIllustration(
  c: ModulesConfig,
  share: number,
  base: QuoteSides
): QuoteSides {
  const i = c.inventory
  if (!i.enabled) return base
  const skew = Math.min(
    i.max_skew_bps,
    Math.floor(
      (i.max_skew_bps * Math.abs(share - i.target_bps)) /
        (i.max_bps - i.target_bps)
    )
  )
  const floored = (bps: number) => Math.max(i.spread_floor_bps, bps)
  const tighter = (bps: number | null) =>
    bps === null ? null : floored(Math.max(0, bps - skew))
  return share >= i.target_bps
    ? {
        buy:
          share >= i.max_bps || base.buy === null
            ? null
            : Math.min(9999, floored(base.buy + skew)),
        sell: tighter(base.sell),
      }
    : {
        buy: tighter(base.buy),
        sell: base.sell === null ? null : floored(base.sell + skew),
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

/** What the bot would quote at `share` and a `rangeBps` price range: the
 * corridor spread, then inventory balancing, then dynamic spreads, composed as
 * `modules::evaluate` composes them, buy capped at 9,999 bps. Leaves out what
 * only a running bot knows: open quotes, funds, vault limits and freshness. */
export function quoteIllustration(
  c: ModulesConfig,
  base: QuoteSides,
  share: number,
  rangeBps: number
): { base: QuoteSides; inventory: QuoteSides; quoted: QuoteSides } {
  const inventory = inventoryIllustration(c, share, base)
  const extra = volatilityIllustration(c, share, rangeBps)
  return {
    base,
    inventory,
    quoted: {
      buy:
        inventory.buy === null || extra.buy === null
          ? null
          : Math.min(9999, inventory.buy + extra.buy),
      sell:
        inventory.sell === null || extra.sell === null
          ? null
          : inventory.sell + extra.sell,
    },
  }
}

type BreakdownKey = 'corridor' | 'inventory' | 'spreads' | 'quoted'
export type QuoteBreakdownRow = {
  key: BreakdownKey
  label: string
  /** False for a module that is switched off. */
  on: boolean
  buy: string
  sell: string
}

const signedPercentage = (bps: number) =>
  bps > 0 ? `+${percentage(bps)}` : bps < 0 ? `−${percentage(-bps)}` : '0%'

/** The Simulate panel's spread table: each step's change per side, and the
 * spread that results. Rebalancing sells inventory but never moves a quote, so
 * it has no row. */
export function quoteBreakdown(
  c: ModulesConfig,
  base: QuoteSides,
  share: number,
  rangeBps: number
): QuoteBreakdownRow[] {
  const { inventory, quoted } = quoteIllustration(c, base, share, rangeBps)
  const change = (
    on: boolean,
    before: number | null,
    after: number | null
  ): string =>
    !on
      ? 'Off'
      : before === null
        ? '—'
        : after === null
          ? 'Paused'
          : signedPercentage(after - before)
  const side = (key: 'buy' | 'sell'): Record<BreakdownKey, string> => ({
    corridor: base[key] === null ? 'Off' : percentage(base[key]),
    inventory: change(c.inventory.enabled, base[key], inventory[key]),
    spreads: change(c.spreads.enabled, inventory[key], quoted[key]),
    quoted:
      quoted[key] !== null
        ? percentage(quoted[key])
        : base[key] === null
          ? 'Off'
          : 'Paused',
  })
  const buy = side('buy')
  const sell = side('sell')
  const rows: [BreakdownKey, string, boolean][] = [
    ['corridor', 'Corridor spread', true],
    ['inventory', moduleNames.inventory, c.inventory.enabled],
    ['spreads', moduleNames.spreads, c.spreads.enabled],
    ['quoted', c.mode === 'live' ? 'Quoted spread' : 'Proposed spread', true],
  ]
  return rows.map(([key, label, on]) => ({
    key,
    label,
    on,
    buy: buy[key],
    sell: sell[key],
  }))
}

/** Why the spread table isn't what the bot quotes today, when it isn't. The
 * bot only applies modules in Live mode. */
export function quoteModeNote(mode: ModulesConfig['mode']): string | null {
  return mode === 'off'
    ? 'Modules are off, so live quotes stay at the corridor spread.'
    : mode === 'shadow'
      ? 'Preview records this proposed spread. Live quotes stay at the corridor spread until you switch to Live.'
      : null
}

export function moduleOverviewState(view: ModulesView, now: number) {
  const fresh = moduleStatusFresh(view.status, view.running, now)
  const c = view.status?.config ?? view.config
  const d = view.status?.decisions.at(-1)?.decision
  if (!view.running)
    return {
      title: 'Ready when you are',
      message: 'The bot is stopped. You can still edit its settings.',
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
      title: isWarmingUp(d)
        ? 'Getting ready'
        : c.mode === 'shadow'
          ? 'Preview is waiting'
          : 'Quotes are paused',
      message: isWarmingUp(d)
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
      message: 'Turn on a module in Parameters to adjust the bot’s behavior.',
      current: true,
    }
  if (d && c.spreads.enabled && isWarmingUp(d))
    return {
      title: 'Getting ready',
      message:
        'Collecting price history. Quotes stay live with the maximum spread buffer until then.',
      current: true,
    }
  return {
    title: 'Your modules are running',
    message: 'Quote adjustments follow the bot’s active settings.',
    current: true,
  }
}

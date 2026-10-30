import { curveConfigErrors, freshReading, shapeLabel } from './curve'
import {
  freshUnwindReading,
  liftLimitNgn,
  minSpreadBps,
  profitUnwindErrors,
} from './unwind'
import { formatAtomic } from './format'
import {
  isWarmingUp,
  moduleStatusFresh,
  type CorridorSpread,
  type CurveModuleConfig,
  type LiquidityConfig,
  type LiquidityStatus,
  type LiquidityStep,
  type ModuleStatus,
  type ModulesConfig,
  type ModulesView,
  type ProfitUnwindConfig,
  type Weekday,
} from './modules'

export type ModuleKey =
  | 'inventory'
  | 'spreads'
  | 'rebalance'
  | 'liquidity'
  | 'curve'
  | 'profit_unwind'
export const moduleNames: Record<ModuleKey, string> = {
  inventory: 'Inventory balancing',
  spreads: 'Dynamic spreads',
  rebalance: 'Spot rebalancing',
  liquidity: 'Liquidity schedule',
  curve: 'Inventory curve',
  profit_unwind: 'Profit unwind',
}
export const moduleKeys: ModuleKey[] = [
  'inventory',
  'spreads',
  'rebalance',
  'liquidity',
  'curve',
  'profit_unwind',
]

/** The modules that can run on this bot's funds, in display order. Every
 * module runs on a vault; a bot that trades from its own wallet has nothing
 * for the others to read or act through, so it gets the inventory curve and
 * profit unwind. Mirrors `ModulesConfig::runnable` in the bot. */
export const modulesFor = (vault: boolean): ModuleKey[] =>
  vault ? moduleKeys : ['curve', 'profit_unwind']

/** Inventory balancing and the inventory curve both lean quotes on what the
 * vault holds, so one stays off while the other runs. The bot refuses both. */
export function leanConflict(
  c: ModulesConfig,
  key: ModuleKey,
  vault: boolean
): string | null {
  if (!vault) return null
  if (key === 'curve' && c.inventory.enabled)
    return 'Inventory balancing is on. Both lean quotes on what the vault holds, so turn it off to run the curve.'
  if (key === 'inventory' && c.curve.enabled)
    return 'The inventory curve is on. Both lean quotes on what the vault holds, so turn it off to run inventory balancing.'
  if (key === 'profit_unwind' && c.inventory.enabled)
    return 'Inventory balancing is on. It narrows a side below your spread, which profit unwind’s round-trip limit counts on, so turn it off to run profit unwind.'
  if (key === 'inventory' && c.profit_unwind.enabled)
    return 'Profit unwind is on. Inventory balancing narrows a side below the spread its round-trip limit counts on, so turn it off to run inventory balancing.'
  return null
}
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
const shownText = (v: unknown) => (typeof v === 'string' && v ? v : '—')
const shownPriceOffset = (v: unknown, unit: string) =>
  typeof v === 'number' && Number.isFinite(v)
    ? `${v.toLocaleString(undefined, { maximumFractionDigits: 2 })} ${unit}`
    : '—'

/** Every setting the Parameters form edits, labeled as the form labels it.
 * Dealer credentials are TOML-only and not listed. */
const editableSettings: readonly [
  string,
  (c: ModulesConfig) => unknown,
  (v: unknown, priceUnit: string) => string,
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
  ['Liquidity schedule', (c) => c.liquidity.enabled, shownSwitch],
  ['Market opens (UTC)', (c) => c.liquidity.open_utc, shownText],
  ['Market closes (UTC)', (c) => c.liquidity.close_utc, shownText],
  [
    'Market days (UTC)',
    (c) => c.liquidity.days.join(','),
    (v) =>
      String(v)
        .split(',')
        .filter(Boolean)
        .map((d) => weekdayNames[d as Weekday] ?? d)
        .join(', ') || '—',
  ],
  [
    'Keep liquid during market hours',
    (c) => c.liquidity.market_floor_bps,
    shownPercent,
  ],
  [
    'Keep liquid off-hours',
    (c) => c.liquidity.off_hours_floor_bps,
    shownPercent,
  ],
  [
    'Roles modifier address',
    (c) => c.liquidity.authority?.roles_modifier ?? '',
    shownText,
  ],
  ['Role key', (c) => c.liquidity.authority?.role_key ?? '', shownText],
  ['Ignore gaps smaller than', (c) => c.liquidity.tolerance_bps, shownPercent],
  ['Inventory curve', (c) => c.curve.enabled, shownSwitch],
  ['Start below the mid', (c) => c.curve.start_below_mid, shownPriceOffset],
  [
    'End above the reference buy',
    (c) => c.curve.end_above_buy,
    shownPriceOffset,
  ],
  ['Curve shape', (c) => c.curve.shape, (v) => shapeLabel(Number(v))],
  ['Profit unwind', (c) => c.profit_unwind.enabled, shownSwitch],
  [
    'Profit unwind quotes',
    (c) => c.profit_unwind.live,
    (v) => (v ? 'Live' : 'Preview'),
  ],
  [
    'Share of gain given back',
    (c) => c.profit_unwind.share,
    (v) => (Number.isFinite(v) ? `${Math.round(Number(v) * 100)}%` : '—'),
  ],
  ['Cap', (c) => c.profit_unwind.cap_ngn, shownPriceOffset],
  ['Ignore gains under', (c) => c.profit_unwind.min_gain_ngn, shownPriceOffset],
  [
    'Start profit unwind at',
    (c) => c.profit_unwind.start_pct,
    (v) => (Number.isFinite(v) ? `${v}% naira` : '—'),
  ],
]

/** What saving would change, in form order. `Object.is` so an empty field
 * (NaN) counts as one change, not a change on every render. */
export const moduleChanges = (
  saved: ModulesConfig,
  draft: ModulesConfig,
  priceUnit = 'corridor units'
): { label: string; from: string; to: string }[] =>
  editableSettings
    .filter(([, read]) => !Object.is(read(saved), read(draft)))
    .map(([label, read, show]) => ({
      label,
      from: show(read(saved), priceUnit),
      to: show(read(draft), priceUnit),
    }))

/** UI validation mirrors the numeric config constraints. The server remains
 * authoritative, including dealer URL and wallet validation. Never repair a
 * related setting silently when a slider moves. */
export function moduleParameterErrors(
  c: ModulesConfig,
  vault = true
): string[] {
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
    ...liquidityScheduleErrors(c.liquidity),
    ...liquidityAuthorityErrors(c.liquidity, c.mode),
    ...curveConfigErrors(c.curve).map((m) => `Inventory curve: ${m}`),
    ...profitUnwindErrors(c.profit_unwind).map((m) => `Profit unwind: ${m}`),
    vault &&
      c.curve.enabled &&
      c.inventory.enabled &&
      'Inventory balancing and the inventory curve both lean quotes on what the vault holds; turn one of them off.',
    vault &&
      c.profit_unwind.enabled &&
      c.inventory.enabled &&
      'Inventory balancing narrows a side below the spread profit unwind’s round-trip limit counts on; turn one of them off.',
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

const integer = (n: number, min: number, max: number) =>
  Number.isInteger(n) && n >= min && n <= max

/** Server defaults for a config saved before the liquidity schedule existed. */
/** Server defaults for a config saved before the inventory curve existed. */
export const defaultCurve = (): CurveModuleConfig => ({
  enabled: false,
  start_below_mid: 0,
  end_above_buy: 0,
  shape: 2,
})
/** The corridor's one bps spread, or null when the two sides differ or
 * either is absolute: the curve then can't run, and says so. */
export function equalSpreadBps(view: Pick<ModulesView, 'corridor_spreads'>) {
  const sides = [view.corridor_spreads.buy, view.corridor_spreads.sell].filter(
    (s) => s !== null
  )
  const first = sides[0]
  return first &&
    sides.every((s) => s.kind === 'bps' && s.value === first.value)
    ? first.value
    : null
}

/** The cap above which the panel warns, at the bot's last readings: the
 * round-trip limit at the corridor's smaller spread and the draft's start,
 * sized off every mid the lift may go out on. Flat that's the feed; with the
 * curve on, the bot may quote either (the curve's reference mid is within 1%
 * of the feed, flat past that), so it's the lower of the two. Null when a mid
 * it needs has no fresh reading, or there's no bps spread. The bot quotes the
 * cap as set. */
export function draftUnwindCapLimit(
  view: ModulesView,
  draft: ModulesConfig,
  now: number
): number | null {
  const reading = freshUnwindReading(view.status?.profit_unwind, now)
  const spread = minSpreadBps(view)
  if (!reading || spread === null) return null
  const curve = draft.curve.enabled
    ? freshReading(view.status?.curve, now)
    : undefined
  if (curve === null) return null
  const mids = [reading.feed_mid, ...(curve ? [curve.reference_mid] : [])]
  return Math.min(
    ...mids.map((mid) =>
      liftLimitNgn(spread, mid, draft.profit_unwind.start_pct)
    )
  )
}

/** Server defaults for a config saved before profit unwind existed. */
export const defaultProfitUnwind = (): ProfitUnwindConfig => ({
  enabled: false,
  live: false,
  share: 0.5,
  cap_ngn: 2,
  min_gain_ngn: 0.7,
  start_pct: 50,
  refresh_secs: 900,
})
export const defaultLiquidity = (): LiquidityConfig => ({
  enabled: false,
  open_utc: '08:00',
  close_utc: '17:00',
  days: ['mon', 'tue', 'wed', 'thu', 'fri'],
  market_floor_bps: 5000,
  off_hours_floor_bps: 0,
  tolerance_bps: 500,
  authority: null,
})
export const weekdays: readonly Weekday[] = [
  'mon',
  'tue',
  'wed',
  'thu',
  'fri',
  'sat',
  'sun',
]
export const weekdayNames: Record<Weekday, string> = {
  mon: 'Mon',
  tue: 'Tue',
  wed: 'Wed',
  thu: 'Thu',
  fri: 'Fri',
  sat: 'Sat',
  sun: 'Sun',
}

/** Minutes after midnight for a 24h "HH:MM" clock, or null. */
export function clockMinutes(hhmm: string): number | null {
  const match = /^([01]\d|2[0-3]):([0-5]\d)$/.exec(hhmm)
  return match ? Number(match[1]) * 60 + Number(match[2]) : null
}

/** A UTC clock time moved by the operator's offset (minutes east of UTC).
 * `dayShift` is -1 or 1 when the local time falls on the previous or next day. */
export function shiftClock(
  hhmm: string,
  offsetMinutes: number
): { time: string; dayShift: number } | null {
  const minutes = clockMinutes(hhmm)
  if (minutes === null) return null
  const shifted = minutes + offsetMinutes
  const local = ((shifted % 1440) + 1440) % 1440
  const pad = (n: number) => String(n).padStart(2, '0')
  return {
    time: `${pad(Math.floor(local / 60))}:${pad(local % 60)}`,
    dayShift: Math.floor(shifted / 1440),
  }
}

/** Schedule rules that hold in every mode, enabled or not. */
export function liquidityScheduleErrors(l: LiquidityConfig): string[] {
  const open = clockMinutes(l.open_utc)
  const close = clockMinutes(l.close_utc)
  const floors =
    integer(l.market_floor_bps, 0, 10000) &&
    integer(l.off_hours_floor_bps, 0, 10000)
  return [
    (open === null || close === null || open === close) &&
      'Liquidity schedule: enter opening and closing times as HH:MM in UTC, and make them different.',
    (l.days.length === 0 ||
      new Set(l.days).size !== l.days.length ||
      l.days.some((day) => !weekdays.includes(day))) &&
      'Liquidity schedule: choose at least one day, each listed once.',
    !floors &&
      'Liquidity schedule: floors must be 0–100% of free settlement, in steps of 0.01%.',
    floors &&
      l.market_floor_bps < l.off_hours_floor_bps &&
      'Liquidity schedule: keep at least as much liquid during market hours as off-hours.',
    !integer(l.tolerance_bps, 1, 5000) &&
      'Liquidity schedule: tolerance must be 0.01–50%, in steps of 0.01%.',
  ].filter((message): message is string => typeof message === 'string')
}

const nonZeroHex = (value: string, digits: number) =>
  new RegExp(`^0x[0-9a-fA-F]{${digits}}$`).test(value) && !/^0x0+$/.test(value)

/** Authority format applies whenever one is entered; Live mode with the
 * schedule enabled needs one, because the bot must be able to send the change. */
export function liquidityAuthorityErrors(
  l: LiquidityConfig,
  mode: ModulesConfig['mode']
): string[] {
  return [
    l.authority &&
      !nonZeroHex(l.authority.roles_modifier, 40) &&
      'Liquidity schedule: the Roles modifier must be a non-zero 0x address (40 hex characters).',
    l.authority &&
      !nonZeroHex(l.authority.role_key, 64) &&
      'Liquidity schedule: the role key must be a non-zero 0x value with 64 hex characters.',
    l.enabled &&
      mode === 'live' &&
      !l.authority &&
      'Liquidity schedule: Live mode needs the Roles modifier and role key that let this bot change the floor.',
  ].filter((message): message is string => typeof message === 'string')
}

export const WEEK_MINUTES = 7 * 1440
/** [start, end) in minutes from Monday 00:00 UTC. */
export type ScheduleSegment = {
  start: number
  end: number
  market: boolean
  floor_bps: number
}

/** The whole UTC week as contiguous market / off-hours segments, or null when
 * the times are invalid. Windows open on each listed day and close at the next
 * `close_utc`, so a close earlier than the open runs past midnight; Sunday's
 * window wraps into Monday. Each window lasts under 24h, so none overlap. */
export function liquiditySchedule(
  l: LiquidityConfig
): ScheduleSegment[] | null {
  const open = clockMinutes(l.open_utc)
  const close = clockMinutes(l.close_utc)
  if (open === null || close === null || open === close) return null
  const length = (close - open + 1440) % 1440
  const windows = [...new Set(l.days)]
    .filter((day) => weekdays.includes(day))
    .flatMap((day): [number, number][] => {
      const start = weekdays.indexOf(day) * 1440 + open
      const end = start + length
      return end <= WEEK_MINUTES
        ? [[start, end]]
        : [
            [start, WEEK_MINUTES],
            [0, end - WEEK_MINUTES],
          ]
    })
    .sort((a, b) => a[0] - b[0])
  const segment = (start: number, end: number, market: boolean) => ({
    start,
    end,
    market,
    floor_bps: market ? l.market_floor_bps : l.off_hours_floor_bps,
  })
  const { segments, cursor } = windows.reduce<{
    segments: ScheduleSegment[]
    cursor: number
  }>(
    (acc, [start, end]) => ({
      segments: [
        ...acc.segments,
        ...(start > acc.cursor ? [segment(acc.cursor, start, false)] : []),
        segment(start, end, true),
      ],
      cursor: end,
    }),
    { segments: [], cursor: 0 }
  )
  return cursor < WEEK_MINUTES
    ? [...segments, segment(cursor, WEEK_MINUTES, false)]
    : segments
}

/** Week segments split per UTC day, with minutes relative to that midnight. */
export function scheduleByDay(segments: ScheduleSegment[]) {
  return weekdays.map((day, i) => {
    const from = i * 1440
    const to = from + 1440
    return {
      day,
      segments: segments
        .filter((s) => s.end > from && s.start < to)
        .map((s) => ({
          ...s,
          start: Math.max(s.start, from) - from,
          end: Math.min(s.end, to) - from,
        })),
    }
  })
}

export const marketMinutes = (segments: ScheduleSegment[]) =>
  segments
    .filter((s) => s.market)
    .reduce((total, s) => total + s.end - s.start, 0)

/** `part / whole` in bps for atomic amounts, or null when either is unknown. */
export function shareBps(
  part: string | null,
  whole: string | null
): number | null {
  if (!part || !whole || !/^\d+$/.test(part) || !/^\d+$/.test(whole))
    return null
  const total = BigInt(whole)
  return total === 0n ? null : Number((BigInt(part) * 10000n) / total)
}

export const LIQUIDITY_STALE_SECS = 45

const liquidityLabels: Record<LiquidityStep, [live: string, preview: string]> =
  {
    hold: ['Floor on schedule', 'Preview · no change'],
    raise_floor: ['Raising the floor', 'Preview · would raise the floor'],
    lower_floor: ['Lowering the floor', 'Preview · would lower the floor'],
    drain: ['Capping new quotes', 'Preview · would cap new quotes'],
    recall: ['Pulling settlement from Aave', 'Preview · would pull from Aave'],
    // Missing vault reads, or a vault state the floor does not apply to.
    wait: ['Waiting', 'Preview · waiting'],
  }

function previewMessage(l: LiquidityStatus, decimals: number): string {
  const amount = (raw: string | null) =>
    raw === null ? 'the scheduled level' : formatAtomic(raw, decimals)
  const would = {
    hold: 'The floor matches the schedule. Nothing would change.',
    raise_floor: `Would raise the floor to ${amount(l.target)}.`,
    lower_floor: `Would lower the floor to ${amount(l.target)}.`,
    drain: `Would cap new quotes so they commit at most ${amount(l.quote_cap ?? l.target)}, wait for live quotes above the new floor to expire, then lower the floor to ${amount(l.target)}.`,
    recall: `Would pull settlement back from Aave until ${amount(l.floor)} is liquid.`,
    wait: l.message,
  }[l.step]
  return `${would} Preview sends no transactions.`
}

/** Overview headline for the liquidity schedule, in the style of
 * `dynamicSpreadState`. Uses the running config, not the saved draft. */
export function liquidityState(
  view: ModulesView,
  now: number
): { label: string; message: string; current: boolean } {
  const status = view.status
  const l = status?.liquidity
  // Older bots report a running config without the liquidity key.
  const running = status?.config.liquidity
  if (!view.running)
    return {
      label: 'Bot stopped',
      message:
        'Start the bot to run the liquidity schedule. Saving settings alone does not start it.',
      current: false,
    }
  if (!status)
    return {
      label: 'No runtime data',
      message:
        'The panel has not received a module status file from this bot yet.',
      current: false,
    }
  if (!l)
    return {
      label: 'No liquidity data',
      message:
        'The bot has not reported a liquidity reading. Bots released before this module never do; update the bot on its card above. Otherwise check that the schedule is enabled and saved.',
      current: false,
    }
  if (l.at > now || now - l.at > LIQUIDITY_STALE_SECS)
    return {
      label: 'Reading stale',
      message:
        'No current liquidity reading. Check the vault RPC reads in the bot logs. The last values are shown below.',
      current: false,
    }
  if (status.config.mode === 'off' || !running?.enabled)
    return {
      label: 'Schedule off',
      message:
        'The running configuration does not move the floor. The last readings are shown for reference.',
      current: false,
    }
  const shadow = status.config.mode === 'shadow'
  const [live, preview] = liquidityLabels[l.step]
  return {
    label: shadow ? preview : live,
    message: shadow ? previewMessage(l, view.settlement_decimals) : l.message,
    current: true,
  }
}

/** Setup advice when the bot cannot change the floor, or null. */
export function liquiditySetupHint(l: LiquidityStatus): string | null {
  const grant = `Grant the role to ${l.sender} and keep it funded with gas.`
  if (
    l.authority_target &&
    l.operator_admin &&
    l.authority_target.toLowerCase() !== l.operator_admin.toLowerCase()
  )
    return `The Roles modifier acts for ${l.authority_target}, but this vault's operator admin is ${l.operator_admin}. Use a Roles modifier enabled on the operator-admin Safe. ${grant}`
  if (l.actions.at(-1)?.ok === false)
    return `The last transaction failed. ${grant}`
  return null
}

/** The settings the bot runs from a saved config. On a wallet bot the
 * vault-only modules stay off whatever the file says. Mirrors
 * `ModulesConfig::runnable` in the bot. */
export function runnableConfig(
  c: ModulesConfig,
  vault: boolean
): ModulesConfig {
  if (vault) return c
  return {
    ...c,
    inventory: { ...c.inventory, enabled: false },
    spreads: { ...c.spreads, enabled: false },
    rebalance: { ...c.rebalance, enabled: false },
    liquidity: { ...c.liquidity, enabled: false },
  }
}

/** Whether the bot runs different settings than the saved ones would give it.
 * Older bots don't report `liquidity`; that only counts when the saved
 * schedule is on. */
export function runningConfigDiffers(view: ModulesView): boolean {
  const running = view.status?.config
  if (!running) return false
  const expected = runnableConfig(view.config, view.vault ?? true)
  if (running.liquidity === undefined) {
    const { liquidity, ...saved } = expected
    return (
      liquidity.enabled || JSON.stringify(saved) !== JSON.stringify(running)
    )
  }
  return JSON.stringify(running) !== JSON.stringify(expected)
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
/** The dynamic-spreads addition before inventory weighting, as
 * `modules::evaluate` takes it: the price range in bps × sensitivity, rounded
 * up and capped. A null range is a window still warming up, which quotes at
 * the cap. Zero when the module is off. */
function dynamicExtra(c: ModulesConfig, rangeBps: number | null): number {
  if (!c.spreads.enabled) return 0
  if (rangeBps === null) return c.spreads.max_extra_bps
  return Math.min(
    c.spreads.max_extra_bps,
    Math.ceil(rangeBps * c.spreads.multiplier)
  )
}

/** Each side's dynamic addition at `share`, as `strategies::spread_additions`
 * splits it, with buys paused at the purchase limit. */
export function volatilityIllustration(
  c: ModulesConfig,
  share: number,
  rangeBps: number | null
) {
  const extra = dynamicExtra(c, rangeBps)
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

/** What the bot would quote at `share` and a `rangeBps` price range (null:
 * dynamic spreads still warming up): the corridor spread, then inventory
 * balancing, then dynamic spreads, composed as `modules::evaluate` composes
 * them, buy capped at 9,999 bps. In Live this is the spread `modules::apply`
 * puts on the RFQ book. Leaves out what only a running bot knows: open quotes,
 * funds, vault limits and freshness. */
export function quoteIllustration(
  c: ModulesConfig,
  base: QuoteSides,
  share: number,
  rangeBps: number | null
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

const quotedSide = (bps: number | null, corridor: number | null) =>
  bps !== null ? percentage(bps) : corridor === null ? 'off' : 'paused'

/** What Live quotes start at while dynamic spreads has too little price
 * history: the full maximum on top of inventory balancing. The bot keeps its
 * window across restarts, but Off mode records none, and a stop longer than
 * the window lets it expire. Null when that start matches the table. */
export function warmupNote(
  c: ModulesConfig,
  base: QuoteSides,
  share: number,
  rangeBps: number
): string | null {
  if (c.mode !== 'live' || !c.spreads.enabled) return null
  const warm = quoteIllustration(c, base, share, null).quoted
  const now = quoteIllustration(c, base, share, rangeBps).quoted
  if (warm.buy === now.buy && warm.sell === now.sell) return null
  return `Until the bot has ${c.spreads.warmup_secs.toLocaleString()} sec of price history, dynamic spreads add their full maximum: buys ${quotedSide(warm.buy, base.buy)}, sells ${quotedSide(warm.sell, base.sell)}. Expect that right after switching from Off, or after a stop longer than the ${c.spreads.window_secs.toLocaleString()} sec window.`
}

type SpreadWindow = NonNullable<
  ModuleStatus['decisions'][number]['inputs']
>['spread_window']

/** The window's price range in bps, `(high / low − 1) × 10,000`, the same
 * float steps as `strategies::spread_window`. Null while the window is
 * warming up, when dynamic spreads quotes at its cap. */
export function windowRangeBps(
  w: Pick<SpreadWindow, 'low' | 'high' | 'extra_bps'>
): number | null {
  return w.extra_bps === null || w.low === null || w.high === null
    ? null
    : (w.high / w.low - 1) * 10000
}

/** What the bot is working from right now, from its latest evaluation. */
export type LiveReading = {
  share: number
  /** Null while dynamic spreads is warming up. */
  rangeBps: number | null
  /** What that evaluation put on the book (Live) or proposed (Preview). */
  quoted: QuoteSides
  /** The corridor spread it started from, to tell an off side from a pause. */
  base: QuoteSides
  mode: 'live' | 'shadow'
}

/** The bot's current holdings, price range and spreads. Null unless its
 * latest evaluation is current, unblocked and recorded its inputs. */
export function liveReading(
  view: ModulesView,
  now: number
): LiveReading | null {
  const status = view.status
  const last = status?.decisions.at(-1)
  if (
    !status ||
    !last?.inputs ||
    last.decision.blocked ||
    last.decision.inventory_bps === null ||
    status.config.mode === 'off' ||
    !moduleStatusFresh(status, view.running, now)
  )
    return null
  return {
    share: last.decision.inventory_bps,
    rangeBps: windowRangeBps(last.inputs.spread_window),
    quoted: { buy: last.decision.buy_bps, sell: last.decision.sell_bps },
    base: { buy: last.inputs.base_buy_bps, sell: last.inputs.base_sell_bps },
    mode: status.config.mode,
  }
}

/** The table's comparison row: what the bot quotes (or proposes) right now. */
export function liveRow(reading: LiveReading): {
  label: string
  buy: string
  sell: string
} {
  const side = (key: 'buy' | 'sell') => {
    const shown = quotedSide(reading.quoted[key], reading.base[key])
    return shown.charAt(0).toUpperCase() + shown.slice(1)
  }
  return {
    label: reading.mode === 'live' ? 'Quoting now' : 'Proposed now',
    buy: side('buy'),
    sell: side('sell'),
  }
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
  // `?.`: an older bot's running config has no liquidity key.
  if (!moduleKeys.some((key) => c[key]?.enabled))
    return {
      title: 'No modules enabled',
      message: 'Turn on a module in Settings to adjust the bot’s behavior.',
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

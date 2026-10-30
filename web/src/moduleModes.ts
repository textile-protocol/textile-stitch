import type { Mode, ModulesConfig } from './modules'

/** Each quote module's run mode, as the bot reads it. Mirrors
 * `ModulesConfig::mode_of` and `normalized` in src/modules/config.rs;
 * `moduleModeParity.json` pins the two together. */

export type ModuleKey =
  | 'inventory'
  | 'spreads'
  | 'rebalance'
  | 'liquidity'
  | 'curve'
  | 'profit_unwind'
/** The bot's `Module::ALL` order. */
export const moduleKeys: readonly ModuleKey[] = [
  'inventory',
  'spreads',
  'rebalance',
  'liquidity',
  'curve',
  'profit_unwind',
]

/** Off while switched off, otherwise the module's own mode, falling back to
 * the shared one. While profit unwind follows the shared mode its older
 * `live` switch still applies, so a shared Live without it previews. */
export function moduleMode(c: ModulesConfig, key: ModuleKey): Mode {
  // `?.`: an older bot's running config has no table for later modules.
  const table = c[key] as ModulesConfig[ModuleKey] | undefined
  if (!table?.enabled) return 'off'
  if (table.mode) return table.mode
  return key === 'profit_unwind' && c.mode === 'live' && !c.profit_unwind.live
    ? 'shadow'
    : c.mode
}

export const isLive = (c: ModulesConfig, key: ModuleKey) =>
  moduleMode(c, key) === 'live'

/** Whether the bot runs its module runtime at all: a shared mode other than
 * Off keeps it recording with every module off. Mirrors `runtime_on`. */
export const runtimeOn = (c: ModulesConfig) =>
  c.mode !== 'off' || moduleKeys.some((key) => moduleMode(c, key) !== 'off')

/** Inventory balancing and dynamic spreads each on only while Live: what
 * quotes are decided with. Mirrors `live_quotes`. */
export const liveQuotes = (c: ModulesConfig): ModulesConfig => ({
  ...c,
  inventory: { ...c.inventory, enabled: isLive(c, 'inventory') },
  spreads: { ...c.spreads, enabled: isLive(c, 'spreads') },
})

/** One of inventory balancing and dynamic spreads runs Live and the other in
 * Preview: quotes carry only the Live one. */
export function quoteModesDiffer(c: ModulesConfig): boolean {
  const inventory = moduleMode(c, 'inventory')
  const spreads = moduleMode(c, 'spreads')
  return inventory !== 'off' && spreads !== 'off' && inventory !== spreads
}

/** `table` with its switch and own mode, keys in the bot's order so the JSON
 * reads the same as the bot's. No own mode leaves the key out. */
function withSwitch<T extends { enabled: boolean; mode?: Mode }>(
  table: T,
  enabled: boolean,
  mode: Mode | undefined
): T {
  const { enabled: _, mode: __, ...rest } = table
  return { enabled, ...(mode ? { mode } : {}), ...rest } as T
}

/** `c` with `shared` as the shared mode, every module keeping its mode.
 * `own` picks what a running module's table names, given what it would run
 * in without a mode of its own. A table an older bot's config leaves out
 * stays out. */
function withModes(
  c: ModulesConfig,
  shared: Mode,
  own: (mode: Mode, follows: Mode) => Mode | undefined
): ModulesConfig {
  const live = isLive(c, 'profit_unwind')
  const next = { ...c, mode: shared }
  for (const key of moduleKeys) {
    const table = c[key] as ModulesConfig[ModuleKey] | undefined
    if (!table) continue
    const mode = moduleMode(c, key)
    const follows =
      key === 'profit_unwind' && shared === 'live' && !live ? 'shadow' : shared
    ;(next as Record<ModuleKey, unknown>)[key] = withSwitch(
      table,
      mode !== 'off',
      mode === 'off' ? undefined : own(mode, follows)
    )
  }
  if (c.profit_unwind) next.profit_unwind = { ...next.profit_unwind, live }
  return next
}

/** The same modes, in the shape earlier releases wrote whenever that shape
 * can hold them: one shared mode, with profit unwind's `live = false` as the
 * one way to preview beside a shared Live. Other mixes keep Preview shared
 * and name Live per module; nothing running is shared Off. Mirrors
 * `ModulesConfig::normalized`. */
export function normalizeModes(c: ModulesConfig): ModulesConfig {
  const running = (keys: readonly ModuleKey[]) =>
    keys.map((key) => moduleMode(c, key)).filter((mode) => mode !== 'off')
  const all = running(moduleKeys)
  const others = running(moduleKeys.filter((key) => key !== 'profit_unwind'))
  const first = all[0]
  const shared: Mode =
    first === undefined
      ? 'off'
      : all.every((mode) => mode === first)
        ? first
        : moduleMode(c, 'profit_unwind') === 'shadow' &&
            others.every((mode) => mode === 'live')
          ? 'live'
          : 'shadow'
  return withModes(c, shared, (mode, follows) =>
    mode === follows ? undefined : mode
  )
}

/** Every running module naming its own mode, so editing one never moves
 * another's. The editor's working form; save `normalizeModes` of it. */
export const expandModes = (c: ModulesConfig): ModulesConfig =>
  withModes(c, c.mode, (mode) => mode)

/** `c` with `key` set to `mode`, in the expanded form. */
export function withModuleMode(
  c: ModulesConfig,
  key: ModuleKey,
  mode: Mode
): ModulesConfig {
  const next = {
    ...c,
    [key]: withSwitch(
      c[key],
      mode !== 'off',
      mode === 'off' ? undefined : mode
    ),
  }
  return key === 'profit_unwind'
    ? {
        ...next,
        profit_unwind: { ...next.profit_unwind, live: mode === 'live' },
      }
    : next
}

/** Same JSON whatever the key order: a table the panel gives its own mode
 * and one the bot wrote can list keys differently. */
export function sameConfig(a: unknown, b: unknown): boolean {
  const sorted = (value: unknown): unknown =>
    Array.isArray(value)
      ? value.map(sorted)
      : value && typeof value === 'object'
        ? Object.fromEntries(
            Object.entries(value)
              .filter(([, v]) => v !== undefined)
              .sort(([x], [y]) => (x < y ? -1 : x > y ? 1 : 0))
              .map(([k, v]) => [k, sorted(v)])
          )
        : value
  return JSON.stringify(sorted(a)) === JSON.stringify(sorted(b))
}

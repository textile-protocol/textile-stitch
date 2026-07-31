// The shapes the panel API returns. Mirrors the serde structs in
// src/panel/http/, which are camelCase on the wire.

export type BotState =
  | 'running'
  | 'created'
  | 'restarting'
  | 'paused'
  | 'exited'
  | 'dead'
  | 'unknown'

export interface WarningBody {
  kind: string
  message: string
  blocksEditing: boolean
}

export interface ConfigBody {
  corridorId: string | null
  corridorLabel: string | null
  chainId: number
  pools: number
  operatorAddress: string | null
  signer: string
}

export interface Bot {
  name: string
  origin: 'panel' | 'compose' | 'adopted' | 'config-only' | string
  layout: string
  container: string | null
  state: BotState
  status: string
  /** Actively quoting. Only `running` — use `canStop` for lifecycle controls. */
  running: boolean
  /**
   * There's a live process to shut down, so offer Stop rather than Start.
   *
   * Broader than `running`: a `restarting` bot isn't quoting between attempts but
   * the restart policy relaunches it, and a `paused` one is frozen mid-tick. The
   * server derives this from the same predicate its own lifecycle code uses, so the
   * list of Docker states lives in one place.
   */
  canStop: boolean
  image: string | null
  createdUnix: number | null
  editable: boolean
  canMigrate: boolean
  migrateBlockedReason: string | null
  canApprove: boolean
  approveBlockedReason: string | null
  config: ConfigBody | null
  warnings: WarningBody[]
}

export interface Fleet {
  bots: Bot[]
  botImage: string
  botsDir: string
}

export interface Corridor {
  id: string
  displayName: string
  networkLabel: string
  chainId: number
  tomlTemplate: string
}

export interface Spread {
  kind: 'bps' | 'abs'
  value: string
}

export interface Sizing {
  totalLiquidity: string
  minSliceDebt: string
  orderSize: string
  maxOrders: string
}

export interface Pair {
  collateral: string
  collateralDecimals: number
  debt: string
  debtDecimals: number
}

export interface Settings {
  rpcUrl: string
  feedUrl: string
  buy: Spread
  sell: Spread
  takerEnabled: boolean
  poolIndex: number
  poolCount: number
  pair: Pair
  buySizing: Sizing
  sellSizing: Sizing
  ttlSecs: number
  tickIntervalSecs: number
  editable: boolean
}

export interface SaveResult {
  settings: Settings
  restarted: boolean
  restartError: string | null
  message: string
}

export interface SessionInfo {
  authenticated: boolean
  identity: string | null
  passwordLogin: boolean
  tailnetLogin: boolean
}

export interface ActionResult {
  bot: Bot
  message: string | null
}

export interface MigrationResult {
  bot: Bot
  message: string
  movedFiles: string[]
  ledgersRecovered: string[]
  ledgerLoss: string | null
  started: boolean
}

export type LogLevel = 'error' | 'warn' | 'info' | 'debug' | 'trace' | 'plain'

export interface LogLine {
  text: string
  stream: 'stdout' | 'stderr'
  level: LogLevel
}

export interface ExitEvent {
  code: number
  ok: boolean
  action: string
}

/** Result of GET /api/updates — registry digest check for bots + the panel itself. */
export interface ImageUpdateInfo {
  /** Image reference the update would pull (e.g. …:latest). */
  targetImage: string
  /** Image the container is on now, when known. */
  currentImage: string | null
  updateAvailable: boolean
  /** Why no update can be offered (local-only image, registry unreachable, …). */
  reason: string | null
}

export interface BotUpdateInfo {
  name: string
  currentImage: string | null
  updateAvailable: boolean
}

export interface UpdatesStatus {
  bot: ImageUpdateInfo
  panel: ImageUpdateInfo
  bots: BotUpdateInfo[]
}

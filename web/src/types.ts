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
  /** Address page on this chain's explorer, when the host is known. */
  explorerUrl: string | null
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
  /** Contracts aren't deployed yet — shown in the picker but not selectable. */
  pendingDeploy: boolean
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
  /** Re-quote when price moves more than this (bps). 0 = every tick. */
  refreshThresholdBps: number
  tickIntervalSecs: number
  /** Empty = quote off the instantaneous feed. */
  twapWindowSecs: string
  /** Empty = bot default (50) when TWAP is on. */
  twapMaxDeviationBps: string
  leanEnabled: boolean
  leanShadow: boolean
  leanFloorBps: string
  leanBaseBps: string
  leanWideBps: string
  editable: boolean
  /** Raw-config gate. The RFQ card is hidden until this is true. */
  rfqPanelUnlocked: boolean
  rfqEnabled: boolean
  rfqUrl: string
  rfqMakerId: string
  rfqValidationContract: string
  rfqCorridor: string
  /** A maker API key is stored on disk. The secret itself is never returned. */
  rfqApiKeySet: boolean
}

export interface RfqEnrollment {
  makerSlug: string
  environment: string
  corridors: string[]
}

export interface SaveResult {
  settings: Settings
  restarted: boolean
  restartError: string | null
  message: string
  enrollment?: RfqEnrollment
}

export type PanelRuntime = 'docker' | 'process'

export interface SessionInfo {
  authenticated: boolean
  identity: string | null
  passwordLogin: boolean
  tailnetLogin: boolean
  /** How the panel supervises bots. Desktop uses `process` (no Docker socket). */
  runtime: PanelRuntime
  /** The panel binary's version, e.g. `0.1.0`. */
  version: string
  /** Full path to the local config data — where the per-bot config folders live on the host. */
  configDir: string
}

export interface ActionResult {
  bot: Bot
  message: string | null
}

/** POST /api/bots — create. */
export interface CreateBotResult {
  bot: Bot
  message: string
  /**
   * Always true today: create does not verify on-chain Permit2. The UI shows
   * the approval handoff; Approve is a no-op when allowances are already set.
   */
  needsPermit2Approval: boolean
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
  /** Newer digest on the update channel than this bot is running. */
  updateAvailable: boolean
  /**
   * Update is allowed. True for on-channel bots that are behind, and for
   * sha-* / bare `sha256:…` pins so they can leave the pin even when the
   * registry check can't prove a newer digest.
   */
  canUpdate: boolean
}

export interface UpdatesStatus {
  bot: ImageUpdateInfo
  panel: ImageUpdateInfo
  bots: BotUpdateInfo[]
}

/** One published build, from GET /api/bots/{name}/versions. */
export interface BotVersion {
  /** Registry tag, e.g. `sha-14cd877`. */
  tag: string
  /** Full reference a rollback would recreate the bot on. */
  image: string
  digest: string | null
  /** Commit timestamp (RFC 3339). Null when GitHub couldn't attribute the tag. */
  publishedAt: string | null
  /** Commit subject for that build. Same best-effort source as `publishedAt`. */
  subject: string | null
  /** The build the container is on right now. */
  current: boolean
}

/**
 * What a version list's order is worth.
 *
 * `commit` — every row was placed by the commit behind its tag, so it really is
 * newest first. `partial` — some rows couldn't be placed (built off another
 * branch, or older than the commit window); they're appended last, and any one
 * of them could be newer than the rows above. `registry` — nothing could be
 * placed (non-GHCR image, private repo, rate limit), leaving the registry's own
 * tag order, which the Distribution spec says is lexical: a set of builds, not
 * a ranking.
 *
 * Only `commit` licenses calling a row the newest.
 */
export type VersionOrdering = 'commit' | 'partial' | 'registry'

export interface BotVersions {
  /** At most 10. Newest first only when `ordering` is `commit`. */
  versions: BotVersion[]
  ordering: VersionOrdering
  currentImage: string | null
  canRollBack: boolean
  /** Why a rollback would be refused — shown instead of the picker. */
  blockedReason: string | null
  /** Why the list is empty, when asking the registry failed. */
  listingError: string | null
}

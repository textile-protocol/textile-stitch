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
  /** Every pair the bot quotes, in pool order: "cNGN / USDT", "wBRL / USDT". */
  pairs: string[]
  networkLabel: string | null
  chainId: number
  pools: number
  operatorAddress: string | null
  signer: string
  /** Address page on this chain's explorer, when the host is known. */
  explorerUrl: string | null
  /**
   * The OperatorVault funding this bot's quotes, when `[vault]` is set. Null
   * means the capital sits in the operator wallet itself.
   */
  vaultAddress: string | null
  /** Explorer page for `vaultAddress`. Null with it, or on an unknown chain. */
  vaultExplorerUrl: string | null
  /**
   * Whether Textile sends this bot quotes, from the config alone. `waiting`
   * covers an unconfirmed email, a flagged maker and no corridor: the config
   * can't tell those apart, and asking the venue rewrites a seated bot's
   * config, so the page doesn't.
   */
  venue: VenueSeat
}

export type VenueSeat = 'not-connected' | 'waiting' | 'seated'

export interface Bot {
  /** The id: config directory and container name. Never changes. */
  name: string
  /** The operator's own name for it, when set. Show this, fall back to `name`. */
  displayName: string | null
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
  /** GitHub release for the running image, e.g. `v0.1.226`. */
  version: string | null
  editable: boolean
  canMigrate: boolean
  migrateBlockedReason: string | null
  canApprove: boolean
  approveBlockedReason: string | null
  /** Bot that must be stopped to unblock approval — this one, or a sibling. */
  approveBlockedBy: string | null
  /**
   * Whether a withdraw can run right now. Stricter than approval: every live
   * process on the wallet has to be down, since even a maker-only bot quotes
   * against the balance a withdraw would drain.
   */
  canWithdraw: boolean
  withdrawBlockedReason: string | null
  /** The bot to stop first: this one, or a sibling quoting from the wallet. */
  withdrawBlockedBy: string | null
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

export interface CorridorList {
  corridors: Corridor[]
  /**
   * `api` when Textile's corridor registry answered, `embedded` when the panel
   * fell back to the corridors compiled into this build.
   */
  source: 'api' | 'embedded'
  /** Why the registry is missing, in operator words. Null when it isn't. */
  warning: string | null
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

export interface PoolSummary {
  index: number
  pair: string
  corridorId: string | null
  corridorLabel: string | null
  /** Sent back on removal: the index alone is not a stable name for a pool. */
  collateral: string
  debt: string
}

export interface Settings {
  rpcUrl: string
  feedUrl: string
  buy: Spread
  sell: Spread
  takerEnabled: boolean
  poolIndex: number
  poolCount: number
  pools: PoolSummary[]
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
  /** Always true. Leftover from the RFQ beta gate. */
  rfqPanelUnlocked: boolean
  /** Always true. Leftover from the RFQ-as-default rollout. */
  rfqDefaultUnlocked: boolean
  /** Public ladder. False is RFQ-only (the production Swap path). */
  bookEnabled: boolean
  rfqEnabled: boolean
  rfqUrl: string
  rfqMakerId: string
  rfqValidationContract: string
  rfqCorridor: string
  /** The OperatorVault the bot trades from; '' when it trades from its own wallet. */
  vaultAddress: string
  /** A maker API key is stored on disk. The secret itself is never returned. */
  rfqApiKeySet: boolean
}

export interface RfqEnrollment {
  makerSlug: string
  environment: string
  corridors: string[]
  flagged?: boolean
}

export interface SaveResult {
  settings: Settings
  restarted: boolean
  restartError: string | null
  message: string
  enrollment?: RfqEnrollment
}

/** What the panel says back about the operator's address. */
export interface RfqEmailResult {
  message: string
  contactEmail: string
  /** True only when this address was already confirmed on an earlier link. */
  emailVerified: boolean
}

export interface RfqStatusResult {
  message: string
  /** The whole gate: confirmed means seated, unless Textile blocked them. */
  emailVerified: boolean
  contactEmail?: string | null
  settings?: Settings
  enrollment?: RfqEnrollment
  /**
   * Present only once verified, when the panel seated the bot and saved the
   * config: whether a running bot was bounced onto it, and why not if it wasn't.
   */
  restarted?: boolean
  restartError?: string | null
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
  /** Always true. New bots start RFQ-only. */
  rfqDefault: boolean
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
  /** GitHub release for this build, e.g. `v0.1.226`. */
  version: string | null
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

export interface TokenAllowance {
  token: string
  symbol: string
  /** Every corridor on this bot that spends the token. */
  corridors: string[]
  reasons: string[]
  required: string
  usesMaxLiquidity: boolean
  /** Current Permit2 allowance, decimal. Null when the read failed. */
  allowance: string | null
  /** Null means unknown, not "no" — the chain read failed. */
  approved: boolean | null
}

export interface Allowances {
  operatorAddress: string | null
  permit2: string
  chainId: number
  tokens: TokenAllowance[]
  readError: string | null
}

// ---------------------------------------------------------------------------
// GET /api/bots/{name}/funding — the wizard's Fund step reads this on a poll.
//
// One read answers both "is the wallet funded?" and "is Permit2 approved?", so
// the two can never disagree. Chain, feed and price failures are fields, never
// status codes: the step keeps rendering and keeps polling.

/** `stable` is a pool's debt token (USDT, USDC…); `soft` is its collateral. */
export type FundingRole = 'stable' | 'soft'

export interface FundingToken {
  role: FundingRole
  symbol: string
  /** Lowercase 0x hex, like `TokenAllowance.token`. */
  token: string
  decimals: number
  /** Atomic-unit integer string. Null when the chain read failed. */
  balance: string | null
  /** The balance as a decimal string, trailing zeros trimmed. Null with `balance`. */
  balanceText: string | null
  /** USDT per 1 token. Stable tokens are pinned at 1. Null when nothing is known. */
  price: number | null
  /** `fixed` for the stable side, `feed` when the pool's feed answered. */
  priceSource: 'fixed' | 'feed' | null
  /** Why `price` is null, in the panel's words. */
  priceError: string | null
  /**
   * This token's pool does not quote against a dollar stable, so no wallet
   * balance and no feed can ever value it. The per-row twin of
   * `FundingGate.unpriceable`, which only speaks for the whole bot: a bot with
   * one dollar corridor and one without leaves that flag false while these rows
   * stay unvalued forever.
   */
  unpriceable: boolean
  /** balance × price. Null when either is null. */
  usd: number | null
  /** `usd >= gate.minTokenUsd`. Null when `usd` is null. */
  funded: boolean | null
  /** The bot's own `stitch approve` will approve this token. */
  approvalNeeded: boolean
  /** Atomic string. Null when the read failed. */
  permit2Allowance: string | null
  /** Null means unknown (read failed), never "no". */
  approved: boolean | null
}

export interface FundingGas {
  /** ETH, BNB, CELO, POL, or `gas` when the chain is not in the panel's table. */
  symbol: string
  balance: string | null
  balanceText: string | null
  price: number | null
  /** `fallback` is the panel's built-in low figure: nothing answered. Show "(estimated)". */
  priceSource: 'textile' | 'coingecko' | 'fallback' | null
  usd: number | null
  /**
   * `usd >= gate.minGasUsd`. On a chain with no price, any non-zero balance
   * counts. Null when the balance read failed.
   */
  ok: boolean | null
}

export interface FundingGate {
  /**
   * Gas covers the approvals still outstanding. That is all the wizard needs
   * to approve and start; the trading money arrives on the bot page, and
   * `fundedTokens` says which sides hold it.
   */
  passes: boolean
  minTokenUsd: number
  minGasUsd: number
  /** Symbols with `funded === true`. */
  fundedTokens: string[]
  /** Symbols with `approvalNeeded` and `approved !== true` (unknown counts as missing). */
  approvalsMissing: string[]
  /** No token side is funded. */
  needsSide: boolean
  /** `gas.ok === false`. */
  needsGas: boolean
  /**
   * This gate can never pass, whatever arrives in the wallet.
   *
   * The panel values a pool's two sides off its debt token, and only when that
   * token is a dollar stable it knows. On a corridor quoted against anything
   * else (Textile lists `cNGN / GD` on Celo, quoted in GoodDollar) both rows
   * come back unpriced and `needsSide` stays true forever. It is a property of
   * the pair, not of the wallet or of a feed, so it never clears by waiting:
   * the Fund step ends here and sends the operator back to pick another pair.
   */
  unpriceable: boolean
}

export interface Funding {
  operatorAddress: string | null
  /**
   * The address the token rows were read at: the OperatorVault when one is
   * connected, else the operator wallet.
   */
  capitalAddress: string | null
  /** Which of the two `capitalAddress` is. */
  capitalSource: 'vault' | 'wallet'
  /** Address page for `capitalAddress`, when the chain has one. */
  capitalExplorerUrl: string | null
  chainId: number
  /** `Celo`, `BSC`…; null for a custom chain. */
  networkLabel: string | null
  /** Address page on this chain's explorer, when the host is known. */
  explorerUrl: string | null
  permit2: string
  /**
   * What the bot quotes against. With a vault that is the vault's quotable
   * inventory, which is not its token balance: money in the yield adapter
   * counts, money queued for a deposit epoch or reserved for a redemption
   * does not.
   */
  tokens: FundingToken[]
  /**
   * What the signer wallet itself holds, when the capital is in a vault. Null
   * without one, where `tokens` is already the wallet.
   *
   * Gas, dust, a mistaken transfer: the balances that die with the key. Priced
   * like `tokens` (same feeds, other address) and never approved for anything.
   */
  walletTokens: FundingToken[] | null
  gas: FundingGas
  gate: FundingGate
  /** First chain error, or why there is no operator address. Null when reads worked. */
  readError: string | null
  /**
   * Why deleting this bot's key would lose money, or null when the wallet is
   * empty enough that removal is only cleanup. The remove route applies the
   * same rule again before it deletes anything.
   */
  removeBlockedBy: string | null
  checkedAtUnix: number
}

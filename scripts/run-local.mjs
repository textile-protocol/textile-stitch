// Run Stitch against the local hardhat chain — all corridors, two-sided.
//
// Regenerates packages/stitch-bot/stitch.local.toml from the *live* deployed
// addresses (they rotate on every docker restart): one [[pools]] block per
// deployed corridor (cNGN/USDT, cNGN/USDC, COPM/USDT, KES/USDT…), each pointing
// at its own price via the app's `/api/price?pair=<key>` endpoint. Funds the
// operator on every token used (the stable for each bid, the soft for each ask)
// with Permit2 approvals, waits for the price endpoint, then runs the bot.
//
//   node packages/stitch-bot/scripts/run-local.mjs
//
// Env: STITCH_PRIVATE_KEY, FEED_URL (base), INDEXER_URL, BLOCKCHAIN_RPC_URL,
// OFFSET_BPS / SELL_OFFSET_BPS (spreads), TOTAL_ORDER_SIZE_USD (per-side
// notional), MIN_ORDER_SIZE_USD (smallest ladder slice).
import { spawn } from 'node:child_process'
import { readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { createPublicClient, createWalletClient, http, parseAbi } from 'viem'
import { privateKeyToAccount } from 'viem/accounts'

const CRATE = fileURLToPath(new URL('..', import.meta.url)) // packages/stitch-bot/
const RPC = process.env.BLOCKCHAIN_RPC_URL || 'http://localhost:8545'
const FEED_BASE = process.env.FEED_URL || 'http://localhost:8916/api/price'
const INDEXER_URL = process.env.INDEXER_URL || 'http://localhost:8916/api'
const PERMIT2 = '0x000000000022D473030F116dDEE9F6B43aC78BA3'
const KEY =
  process.env.STITCH_PRIVATE_KEY ||
  '0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d'
const BUY_OFFSET_BPS = Number(process.env.OFFSET_BPS || 50)
const SELL_OFFSET_BPS = Number(process.env.SELL_OFFSET_BPS || 50)
const TOTAL_ORDER_SIZE_USD = Number(
  process.env.TOTAL_ORDER_SIZE_USD || process.env.ORDER_SIZE_USD || 2000
) // each side quotes roughly this many USD
const MIN_ORDER_SIZE_USD = Number(process.env.MIN_ORDER_SIZE_USD || 10)
const MAX_LADDER_ORDERS = Number(process.env.MAX_LADDER_ORDERS || 40)
const positiveIntEnv = (name, fallback) => {
  const value = Number.parseInt(process.env[name] || '', 10)
  return Number.isFinite(value) && value > 0 ? value : fallback
}
const FEED_WAIT_SECONDS = positiveIntEnv('FEED_WAIT_SECONDS', 600)
const FEED_PROBE_TIMEOUT_MS = positiveIntEnv('FEED_PROBE_TIMEOUT_MS', 8000)

const addrs = JSON.parse(
  readFileSync(`${CRATE}../constants/src/addresses.localhost.json`)
)
const REACTOR = addrs.SETTLEMENT_V3_FILLER_REACTOR
if (!REACTOR) {
  console.error('Missing filler reactor — deploy v3 + filler first.')
  process.exit(1)
}

// Same corridor map as the app's /api/price endpoint.
const CORRIDORS = {
  'cngn-usdt': {
    oracle: 'SETTLEMENT_V3_USDT_CNGN_ORACLE',
    soft: 'SETTLEMENT_V3_CNGN_MOCK',
    stable: 'SETTLEMENT_V3_USDT',
  },
  'cngn-usdc': {
    oracle: 'SETTLEMENT_V3_USDC_CNGN_ORACLE',
    soft: 'SETTLEMENT_V3_CNGN_USDC_COLLATERAL',
    stable: 'SETTLEMENT_V3_USDC',
  },
  'copm-usdt': {
    oracle: 'SETTLEMENT_V3_USDT_COPM_ORACLE',
    soft: 'SETTLEMENT_V3_COPM_USDT_COLLATERAL',
    stable: 'SETTLEMENT_V3_USDT',
  },
}

const chain = {
  id: 31337,
  name: 'local',
  nativeCurrency: { name: 'ETH', symbol: 'ETH', decimals: 18 },
  rpcUrls: { default: { http: [RPC] } },
}
const pub = createPublicClient({ chain, transport: http(RPC) })
const erc20 = parseAbi([
  'function decimals() view returns (uint8)',
  'function balanceOf(address) view returns (uint256)',
  'function allowance(address,address) view returns (uint256)',
  'function mint(address,uint256)',
  'function approve(address,uint256) returns (bool)',
])
const oracleAbi = parseAbi(['function buyRate() view returns (uint256)'])
const pushOracleAbi = parseAbi([
  'function owner() view returns (address)',
  'function lastBuyPriceRay() view returns (uint256)',
  'function lastSellPriceRay() view returns (uint256)',
  'function update(uint256 buyRay, uint256 sellRay)',
])

// Local PushOracles go stale 30 min after their last update (no keepalive job
// runs locally), and then every read — including the /s/trade rate and this
// bot's feed — reverts. Re-push each forward oracle's current price unchanged
// (Δ=0, so no breaker trips); refreshing the forward side unsticks the reverse
// inverter too. Impersonation only works on a test node, so this is local-only.
const ZERO = '0x0000000000000000000000000000000000000000'
async function refreshOracles() {
  const oracles = [
    ...new Set(
      Object.entries(addrs)
        .filter(
          ([k, v]) =>
            /^SETTLEMENT_V3_.*_ORACLE$/.test(k) &&
            typeof v === 'string' &&
            v &&
            v.toLowerCase() !== ZERO
        )
        .map(([, v]) => v.toLowerCase())
    ),
  ]
  let refreshed = 0
  for (const oracle of oracles) {
    let owner, buy, sell
    try {
      // Probe: a forward PushOracle exposes owner()/lastBuyPriceRay(); an
      // OracleInverter reverts. Inverters derive from a forward, so skip them.
      ;[owner, buy, sell] = await Promise.all([
        pub.readContract({
          address: oracle,
          abi: pushOracleAbi,
          functionName: 'owner',
        }),
        pub.readContract({
          address: oracle,
          abi: pushOracleAbi,
          functionName: 'lastBuyPriceRay',
        }),
        pub.readContract({
          address: oracle,
          abi: pushOracleAbi,
          functionName: 'lastSellPriceRay',
        }),
      ])
    } catch {
      continue // not a forward PushOracle — nothing to push
    }
    try {
      await pub.request({
        method: 'hardhat_setBalance',
        params: [owner, '0x56BC75E2D63100000'],
      })
      await pub.request({
        method: 'hardhat_impersonateAccount',
        params: [owner],
      })
      const w = createWalletClient({
        account: owner,
        chain,
        transport: http(RPC),
      })
      const hash = await w.writeContract({
        address: oracle,
        abi: pushOracleAbi,
        functionName: 'update',
        args: [buy, sell],
      })
      await pub.waitForTransactionReceipt({ hash })
      await pub.request({
        method: 'hardhat_stopImpersonatingAccount',
        params: [owner],
      })
      refreshed += 1
    } catch (e) {
      console.warn(
        `  oracle ${oracle} push failed: ${String(e.shortMessage || e).slice(0, 70)}`
      )
    }
  }
  return refreshed
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

// Keep oracles fresh: once now, then well inside the 30-min staleness window.
console.log(`Refreshing oracles… (${await refreshOracles()} forward oracles)`)

// Resolve every deployed corridor: decimals + the current price, and tally how
// much of each token the operator must hold (bid → stable, ask → soft).
const pools = []
const need = {} // tokenAddr → bigint atomic
const addNeed = (token, amt) => {
  need[token] = (need[token] || 0n) + amt
}
for (const [key, c] of Object.entries(CORRIDORS)) {
  const oracle = addrs[c.oracle]
  const soft = addrs[c.soft]
  const stable = addrs[c.stable]
  if (!oracle || !soft || !stable) continue // corridor not deployed locally
  const read = () =>
    Promise.all([
      pub.readContract({ address: soft, abi: erc20, functionName: 'decimals' }),
      pub.readContract({
        address: stable,
        abi: erc20,
        functionName: 'decimals',
      }),
      pub.readContract({
        address: oracle,
        abi: oracleAbi,
        functionName: 'buyRate',
      }),
    ])
  try {
    // A just-refreshed inverter can momentarily revert on a cold start; retry once.
    const [softDec, stableDec, br] = await read().catch(async () => {
      await sleep(800)
      return read()
    })
    const sd = Number(softDec)
    const dd = Number(stableDec)
    // stable per soft (USDT per cNGN) — same inversion as /api/price, which the
    // bot quotes off. buyRate is collateral-per-debt, so invert it; using the
    // un-inverted value made askSoftHuman ~1, producing zero-size ask slices.
    const price = ((1e27 / Number(br)) * 10 ** sd) / 10 ** dd // stable per soft
    const bidSizeAtomic = BigInt(TOTAL_ORDER_SIZE_USD) * 10n ** BigInt(dd) // stable
    const minBidSizeAtomic = BigInt(MIN_ORDER_SIZE_USD) * 10n ** BigInt(dd)
    const askSoftHuman = Math.max(1, Math.round(TOTAL_ORDER_SIZE_USD / price))
    const askSizeAtomic = BigInt(askSoftHuman) * 10n ** BigInt(sd) // soft
    const minAskDebtAtomic = BigInt(MIN_ORDER_SIZE_USD) * 10n ** BigInt(dd)
    addNeed(stable, bidSizeAtomic)
    addNeed(soft, askSizeAtomic)
    pools.push({
      key,
      soft,
      stable,
      sd,
      dd,
      bidSizeAtomic,
      minBidSizeAtomic,
      askSizeAtomic,
      minAskDebtAtomic,
      askSoftHuman,
      price,
    })
  } catch (e) {
    console.warn(
      `  skip ${key}: oracle read failed (${String(e.shortMessage || e).slice(0, 48)})`
    )
  }
}
if (pools.length === 0) {
  console.error('No corridors deployed locally.')
  process.exit(1)
}

// Fund the operator on every token used (mint if short, approve Permit2).
const account = privateKeyToAccount(KEY)
const wallet = createWalletClient({ account, chain, transport: http(RPC) })
const MAX = (1n << 256n) - 1n
for (const [token, amt] of Object.entries(need)) {
  const want = amt * 100n // headroom for many re-quotes
  const [bal, allow] = await Promise.all([
    pub.readContract({
      address: token,
      abi: erc20,
      functionName: 'balanceOf',
      args: [account.address],
    }),
    pub.readContract({
      address: token,
      abi: erc20,
      functionName: 'allowance',
      args: [account.address, PERMIT2],
    }),
  ])
  if (bal < want) {
    const hash = await wallet.writeContract({
      address: token,
      abi: erc20,
      functionName: 'mint',
      args: [account.address, want],
    })
    await pub.waitForTransactionReceipt({ hash })
  }
  if (allow < want) {
    const hash = await wallet.writeContract({
      address: token,
      abi: erc20,
      functionName: 'approve',
      args: [PERMIT2, MAX],
    })
    await pub.waitForTransactionReceipt({ hash })
  }
}

let toml = `# Auto-generated by run-local.mjs — do not edit; addresses rotate per docker restart.
chain_id = 31337
rpc_url = "${RPC}"
indexer_url = "${INDEXER_URL}"
permit2 = "${PERMIT2}"
reactor = "${REACTOR}"
tick_interval_secs = 5

[feed]
url = "${FEED_BASE}?pair=cngn-usdt"
staleness_secs = 60
`
for (const p of pools) {
  toml += `
[[pools]]
collateral = "${p.soft}"
collateral_decimals = ${p.sd}
debt = "${p.stable}"
debt_decimals = ${p.dd}
feed_url = "${FEED_BASE}?pair=${p.key}"
buy_offset_bps = ${BUY_OFFSET_BPS}
buy_total_liquidity_debt = "${p.bidSizeAtomic}"
buy_min_slice_debt = "${p.minBidSizeAtomic}"
buy_max_orders = ${MAX_LADDER_ORDERS}
sell_offset_bps = ${SELL_OFFSET_BPS}
sell_total_liquidity_collateral = "${p.askSizeAtomic}"
sell_min_slice_debt = "${p.minAskDebtAtomic}"
sell_max_orders = ${MAX_LADDER_ORDERS}
ttl_secs = 300
# 0 → re-sign with a fresh Permit2 nonce every tick. The local oracle is flat,
# so a price-gated bot would never rotate the nonce, and a filled order would
# linger in the book and revert the next fill with InvalidNonce. (Production
# uses a real threshold — the live feed moves, so nonces rotate on their own.)
refresh_threshold_bps = 0
`
}

const cfgPath = `${CRATE}stitch.local.toml`
writeFileSync(cfgPath, toml)
console.log(`Wrote ${cfgPath} — ${pools.length} corridor(s), two-sided:`)
for (const p of pools) {
  console.log(
    `  ${p.key}: bid ${TOTAL_ORDER_SIZE_USD} stable / ask ~${p.askSoftHuman} soft, min ${MIN_ORDER_SIZE_USD} stable (feed ${FEED_BASE}?pair=${p.key})`
  )
}
console.log('')

// A bot with no reachable feed silently skips every pool ("feed fetch failed")
// and posts nothing. Make sure the app's price endpoint is up before starting.
let lastFeedError = ''
async function feedReachable() {
  try {
    // The web dev server (8916) serves /api/price after a cold Vite build, and
    // the first response does 3 chain reads — well over a 1.5s budget — so give
    // each probe room. The app's healthcheck only gates the API on :10000, not
    // this web port, so we must wait it out here.
    const url = `${FEED_BASE}?pair=cngn-usdt`
    const r = await fetch(url, {
      signal: AbortSignal.timeout(FEED_PROBE_TIMEOUT_MS),
    })
    if (r.ok) {
      lastFeedError = ''
      return true
    }
    const body = await r.text().catch(() => '')
    lastFeedError = `${r.status} ${r.statusText}${
      body ? `: ${body.slice(0, 240)}` : ''
    }`
    return false
  } catch (error) {
    lastFeedError = error instanceof Error ? error.message : String(error)
    return false
  }
}
let feedUp = false
for (let i = 0; i < FEED_WAIT_SECONDS; i++) {
  if (await feedReachable()) {
    feedUp = true
    break
  }
  if (i > 0 && i % 15 === 0) {
    const detail = lastFeedError ? `; last error: ${lastFeedError}` : ''
    console.log(`  still waiting for the price endpoint… (${i}s${detail})`)
  }
  await sleep(1000)
}
if (!feedUp) {
  const detail = lastFeedError ? ` Last error: ${lastFeedError}.` : ''
  console.error(
    `\nPrice endpoint did not respond at ${FEED_BASE}?pair=cngn-usdt after ` +
      `${FEED_WAIT_SECONDS}s.${detail} ` +
      `The app container can be healthy before the Redwood web dev server ` +
      `serves /api/price; increase FEED_WAIT_SECONDS if this host is still ` +
      `building.`
  )
  process.exit(1)
}
console.log(`Using price endpoint ${FEED_BASE}.\n`)

// Keep the oracles fresh for the whole session (they'd otherwise re-stale after
// 30 min and the feed/UI would start reverting again).
const refreshTimer = setInterval(
  () => {
    refreshOracles().catch(() => {})
  },
  20 * 60 * 1000
)

const bot = spawn(
  'cargo',
  [
    'run',
    '--quiet',
    '--manifest-path',
    `${CRATE}Cargo.toml`,
    '--',
    '--config',
    cfgPath,
  ],
  {
    stdio: 'inherit',
    env: {
      ...process.env,
      STITCH_PRIVATE_KEY: KEY,
      RUST_LOG: process.env.RUST_LOG || 'info',
    },
  }
)
function cleanup(code) {
  clearInterval(refreshTimer)
  process.exit(code ?? 0)
}
bot.on('exit', cleanup)
process.on('SIGINT', () => cleanup(130))
process.on('SIGTERM', () => cleanup(143))

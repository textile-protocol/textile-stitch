# Signed RFQ reservation lifecycle (ST-01)

Revalidated against `origin/dev` at `6ac0eb612f5300d635d078722e596cbdec194d82`.
Closing an RFQ's API accept window does not revoke the maker's Permit2 order.
The taker can retain its bytes and signature and execute before the signed
deadline. Releasing that stock early lets a second taker receive a firm quote
against the same inventory.

The API still marks the venue order `CANCELLED` and rejects subsequent API
submission. It retains the exact nonce reservation. Funding admission counts
unfilled RFQ orders, including cancelled, expired and superseded rows, until
signed deadline plus the existing 30-second venue/chain clock-skew buffer.
A reconciled `FILLED` order stops counting. Selected replies are excluded from
the separate in-flight aggregate, so each nonce is counted once.

Stitch treats `quoteExpired` as a window-close notice. Selection or cancellation
marks a claim exposed; later losing-result frames cannot release it. A live
RFQ id cannot mint another signature and replace its original claim. Confirmed
never-exposed losers still release immediately. Reload treats all persisted
claims as potentially exposed, since selection can happen during a disconnect.
This requires no ledger-format migration. Uncertain losers after restart may
reserve capacity unnecessarily, but only until deadline plus skew. Stitch does
not infer consumption from a transaction hash; its local claim remains until
expiry or the existing verified vault epoch invalidation.

The responder serializes frame handling and keeps its ledger across socket
reconnects. The API preserves its RFQ row lock and maker advisory locks, so
cancel/submit and concurrent funding admissions retain their existing ordering.
Module-rebalance control-frame exclusions, signature payloads, contract ABI,
nonce construction, and vault policies are unchanged.

## Verification

Regression assertions failed against the original code: early cancellation
removed the Stitch claim, restored full published depth, and released the
selected API nonce. They pass with this change. Tests use the actual Stitch
signer/dispatcher and persistent ledger, Fastify cancellation and submission
routes, PostgreSQL transactions, maker session sending/replay, and the indexed
fill reconciler. Chain-reader responses and socket I/O are synthetic; this is
not a demonstration of two real on-chain fills.

The suite covers deadline/skew boundaries, unused-capacity admission, losing
quotes, duplicate cancellation/fill delivery, stale result frames, repeated
requests, reconnect/restart, failed/slow signing, exact nonce isolation, and
module/vault regressions. Wallet coverage includes real synthetic EOA signing,
vault code paths, and delayed-signing tests. Safe, ERC-4337, custody/MPC and
hardware devices were reasoned through, not physically exercised: no signing,
approval, broadcasting or transaction-confirmation behavior changes here.

## Funding admission and disclosure

For non-admin exact-output requests, the engine reserves the full taker debit
(including fees) in the same database transaction that saves the selected
signed orders. The taker balance snapshot is read before solicitation; the
existing per-wallet lock and released-after-snapshot guard serialize concurrent
admissions. The obligation runs through the latest signed deadline plus skew.
No maker receives `selected` before this transaction commits.

An insufficient balance therefore produces `no_quote` inside the engine and
returns the existing insufficient-funds HTTP error. These candidates never
became stored executable orders or selected maker claims, so their inventory
and exact nonces release through the ordinary no-quote path. GET/status,
submit, cancellation and retries cannot recover their signed payloads.
An HTTP error after selection does not prove non-exposure and keeps the holds.
This does not add an early-release exception to cancellation or `evenIfOpen`.

The regression reproduces the previous failed funding check leaving a stored
signed order. Real Fastify, signed synthetic maker replies and PostgreSQL tests
cover concurrent admissions, polling/submission/cancellation while a funding
lock is held, nonce reuse after rejection, funded cancellation, transaction
rollback and a response-construction error after selection. Only external
balance/nonce reads, the fee rate and socket I/O are synthetic.

## Rollout and limits

Ship the API and Stitch updates together under a controlled quote-intake pause.
Drain all previously exposed signatures through their signed deadlines plus
30 seconds before resuming, because older builds may already have deleted
local claims or released nonce holds. Preserve each bot's ledger directory.
Verify the new API commitment accounting and upgraded makers before reopening
intake. Upgrade every API instance first, then every Stitch maker, while intake
remains paused. Mixed old/new versions are not the completed rollout. No
contract deployment, migration, key rotation or fund movement is needed or
performed by this change.

The legacy API response `status: released` means the venue window closed; it
must not be interpreted as on-chain revocation. The no-quote retry hint
(`reservedUntil`/`retryAfterMs`) reads the same unfilled RFQ orders with the
same deadline-plus-skew cutoff, so a cancelled quote still gets a countdown
instead of "no makers". It reports the hold; it never releases inventory. Arbitrary revocation and spent-nonce detection are not
new early-release paths. Disk-write failure handling and deliberately shared
v1/RFQ wallet budgets retain their existing behavior and need separate work.

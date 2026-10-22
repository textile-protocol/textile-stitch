# Parameter change log

Stitch records every change to the parameters that shape its quotes: the parameter, its value before and after, when it changed (UTC), who changed it and through which path. Nothing has to be switched on for that. The records stay on the machine the bot runs on unless the operator opts in to shipping them to Textile.

Textile uses this for clients whose agreement requires a change log, with an agreed range per parameter (the envelope) and a register of written approvals for anything outside it. The envelope and the approvals live with Textile; a Stitch run by anyone else just has its local log.

## What counts as a parameter

Values are the ones the bot runs with, not the text in the file. A parameter you never set reads as its default, so deleting `max_loss_bps = 500` records a change from 500 to 1500.

Per pool (scope `pool:<collateral>/<debt>`, labelled with the corridor name):

| Parameter | Unit |
|---|---|
| `buy_offset_bps`, `sell_offset_bps` | bps |
| `buy_offset_abs`, `sell_offset_abs` | price (collateral per debt) |
| `buy_total_liquidity_debt`, `sell_total_liquidity_collateral` | token atomic units |
| `buy_order_size_debt`, `sell_order_size_collateral` | token atomic units |
| `feed_url` | the pool's own feed, or the bot-level `[feed].url` it rides |
| `rfq_staleness_secs` | the RFQ freshness window the responder enforces, after clamping |

Bot-wide (scope `bot`): `max_loss_bps`, `max_jump_bps`, `settle_secs`, `staleness_secs`, `rfq_enabled`, `book_enabled`, `modules_enabled`, `change_log.ship`, and every `modules.*` setting while modules are on.

Textile records the venue side the same way: a corridor's `mode`, `quoteTtlMs` and `replyBudgetMs` (scope `corridor:<slug>`), and the price source behind a feed (scope `price-source:<currency or pair>`).

Credentials in URLs never make it into a record. A query value whose name looks like a key, and any `user:password@`, becomes `REDACTED#` plus eight hex digits of its SHA-256, so rotating a key still shows as a change.

## How each change is caught

| Path | Recorded as | Actor |
|---|---|---|
| Panel Settings save | `panel.settings` | `tailnet:<login>`, or `panel password (shared)` |
| Panel raw config editor | `panel.raw` | same |
| Add / remove a corridor | `panel.pools.add` / `panel.pools.remove` | same |
| Modules tab, Modules switch | `panel.modules` / `panel.modules.enabled` | same |
| Connect, email confirm, vault, FX hedge | `panel.enroll`, `panel.verify`, `panel.vault`, `panel.perps` | same |
| A failed live restart putting the old config back | `panel.rollback` | same |
| New bot from the wizard | `panel.create` | same |
| Anything else: an editor on the host, `stitch connect`, a restored backup | `host-edit` | `unrecorded (host edit)` |
| First time a bot is seen | `baseline` | `baseline` |

A password login is shared, so a password session can't name a person. Panels that need a name on every record should sign in through the tailnet.

Hand edits are caught with a snapshot. `change-log.snapshot` holds the config as of the last record. Before the panel saves anything it compares the file with the snapshot and records any difference as a host edit first, so an earlier hand edit isn't pinned on whoever saved next. The bot does the same comparison every time it starts. Stitch reads its config once, at startup, so a change can't take effect without passing that check. A host edit's `changedAt` is the file's mtime; `detectedAt` is when the bot or panel noticed.

## Where records go

The log sits in the bot's directory, beside `stitch.toml`:

```text
change-log.jsonl      one record per line, append-only, never truncated
change-log.snapshot   the config as of the last record
change-log.shipped    how far Textile has acknowledged (only when shipping)
change-log.refused    records Textile refused, if it ever does
change-log.lock       held while comparing and recording
```

Beside the config rather than in a subfolder on purpose. The panel writes these files as root into a directory the bot's user owns, so it opens each one without following a link, refuses anything that isn't a regular file with one name, and hands it to the bot's user through the open handle. A subfolder would be one more path component the bot could swap for a link.

Flat-layout bots (several `stitch.<name>.toml` in one folder) get `stitch.<name>.toml.change-log.*` instead. Their containers can't see those files, so the bot and the panel keep separate logs; migrate to the per-bot layout first if the log matters.

### Shipping to Textile

Off unless the config says:

```toml
[change_log]
ship = true
```

With it on, the bot sends new records to `POST /v2/maker/stitch/parameter-changes` at startup and every minute, signed in with its maker API key. Textile files them under the maker the key belongs to, never under anything the body claims, and stores each record once however often it's re-sent. The panel never calls Textile for this, with one exception for bots that ship. Textile files each record under whichever maker's key ships it, so before a new Connect, a vault switch or a pasted key replaces a shipping bot's key, the panel (or `stitch connect`) delivers what the bot owes under the current key, and refuses the change if it can't.

Turning `ship` off is itself a change, so the bot ships everything up to and including that record before it stops shipping, however the switch was made. Changes made after it, with shipping off, stay local.

If Textile refuses a batch outright (a value past the API's size limits, say), the bot resends it one record at a time and skips only the record that's refused, logging an error and listing it in `change-log.refused`. That record stays in the local journal, and the panel won't delete the config while any are listed. Anything else (Textile unreachable, a 5xx) leaves the cursor where it was and the bot tries again a minute later.

With `ship = true` the bot also refuses to start if it can't record, for example because the directory beside `stitch.toml` isn't writable. An audited bot running a change nobody recorded is the thing this exists to prevent. The panel never refuses a save over the log: if recording fails it says so in the response, and the bot records the change as a host edit when it next starts. The panel also refuses to delete the files of a shipping bot while it has records Textile hasn't acknowledged.

## For Textile: envelopes, approvals and the monthly export

Everything below is admin GraphQL (`@isAdmin`, checked against the database on each call).

`createStitchEnvelope` stores one version of a client's envelope: the maker, optionally one bot (`instanceId`), the agreement reference, a min / max / allowed list / `allowUnset` per parameter, and the venue corridors and price sources that are part of this Stitch. Corridor slugs have to exist. Price sources are named the way the writes record them, as a currency (`ZAR`) for an HTTP or Binance feed or a pair (`cngn-usdt`) for a built-in one; case doesn't matter. Versions are never edited. A correction is a new version with a later `effectiveFrom`, and each change is judged against the version in force when it happened. A bot-specific version wins over a maker-wide one, and which bot a record came from is the instance id the bot reports. That's fine within one operator's fleet, but don't rely on per-bot envelopes to keep one bot of a maker away from another's looser range. Creating a version records a baseline of the corridors and price sources it claims.

`createStitchApproval` enters a written approval: its reference, the parameter, the approved range or values, the window it's valid for, who at the client approved it and where the email or document is. Enter it before making the change: a change only matches an approval entered by the time the change took effect, whatever its validity window says. The same goes for envelope versions, so neither can be back-dated onto a change after the fact.

Every record gets a verdict when it's stored, and it's never recomputed. It's judged at the moment the change took effect: for a host edit that's when the bot started on it (`detectedAt`), not the file's mtime, and never later than when Textile received it.

- `INSIDE_ENVELOPE`: inside every bound on that parameter.
- `APPROVED`: outside, or not covered, but an approval covered the new value at that time. The export names it.
- `OUTSIDE_ENVELOPE`: outside with no approval. Logged at error level and sent to Sentry with the maker, parameter, values and actor.
- `NO_ENVELOPE`: nothing covers the parameter. Logged as a warning when the client has an envelope, because that means the envelope is missing a bound.

Changes aren't blocked, only flagged. Hand edits can't be blocked at all, so the record has to be complete either way. Blocking in the panel or the bot would need a live copy of the envelope on a host that also ships to third parties, and several of these parameters (size caps, spreads, the feed) are the levers ops pull in an incident, where refusing a protective change is worse than flagging it.

For the monthly report, `stitchParameterChangeExport(makerSlug, from, to, window: RECEIVED_AT, format: CSV)` returns the file. `from` is inclusive and `to` exclusive, so pass the first of the month and the first of the next. Use `RECEIVED_AT` for the report: a bot that was offline ships late, and a change made on 31 October that arrives on 2 November would otherwise miss both reports. By receipt, every record is in exactly one report, and the `changed_at_utc` column still says when it happened. For a request about what changed in a range, leave `window` at its default (`CHANGED_AT`). From a shell with database access:

```sh
yarn rw exec exportStitchChanges --maker=acme --from=2026-11-01 --to=2026-12-01 --by=received --out=acme-2026-11.csv
```

The CSV has one row per parameter change, oldest first, every time in UTC, with the verdict, the envelope version and, for approved changes, the approval reference, approver and document. Cells a spreadsheet would run as a formula are prefixed with `'`. Rows are found by the maker slug stored on each one, so a maker's history still exports after the maker itself is offboarded; the `maker_id` column tells two makers apart if a slug is ever reused.

## Retention

Nothing prunes the log. In the API the four tables (`stitch_parameter_change`, `stitch_envelope`, `stitch_envelope_bound`, `stitch_approval`) refuse `UPDATE`, `DELETE` and `TRUNCATE` at the database level. On the host the journal is append-only. Keeping the database itself for the term plus two years is a backup and deletion-protection question for the infrastructure, not for this code.

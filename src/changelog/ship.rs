// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Shipping the journal to Textile. Opt-in: only when `[change_log] ship`.
//!
//! The bot is the shipper, not the panel. Textile's own hosts run bots with
//! no panel beside them, and the bot already holds the one credential
//! Textile recognises for it (the RFQ maker key), so the API can tell which
//! maker a record belongs to without trusting anything in the body.
//!
//! `change-log.shipped` is the byte offset of the first line Textile hasn't
//! acknowledged. It only moves forward after a 2xx, so a lost response means
//! the batch is sent again; the API stores each record id once.
//!
//! Whether to ship is read from the config and the journal, never from the
//! snapshot: a panel save moves the snapshot before the bot restarts, so by
//! the time the bot starts on a config that switched shipping off, the
//! snapshot already agrees. The journal still holds the switch-off record.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;

use super::files::{self, LogFile};
use super::record::ChangeRecord;
use crate::config::Config;

pub const ENDPOINT_PATH: &str = "/v2/maker/stitch/parameter-changes";

/// Records per request. The API refuses more.
pub const MAX_BATCH: usize = 200;

/// How often a running bot looks for records the panel appended while it ran
/// (a save whose restart failed, a pool removal on a stopped bot).
pub const INTERVAL: Duration = Duration::from_secs(60);

/// Whether and how long to ship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    /// Shipping is on: send what's pending now and keep watching.
    Continuous,
    /// Shipping is off now, but Textile hasn't received the record of
    /// switching it off. Send everything up to and including that record (the
    /// byte offset just past it), then stop. Without this the switch-off is
    /// the one change Textile would never see. Records made after it, with
    /// shipping off, stay local.
    FlushUntil(u64),
}

/// `opt_out_end` is [`unshipped_opt_out`] for the journal.
pub fn mode(ship_now: bool, opt_out_end: Option<u64>) -> Mode {
    match (ship_now, opt_out_end) {
        (true, _) => Mode::Continuous,
        (false, Some(end)) => Mode::FlushUntil(end),
        (false, None) => Mode::Off,
    }
}

/// Whether a record switched shipping off.
fn is_opt_out(record: &ChangeRecord) -> bool {
    record.parameter == "change_log.ship"
        && record.before.as_deref() == Some("true")
        && record.after.as_deref() == Some("false")
}

/// Pure: where the newest unshipped switch-off record ends, if any.
pub fn opt_out_end(journal: &[u8], from: u64) -> Option<u64> {
    pending(journal, from, usize::MAX)
        .entries
        .iter()
        .rev()
        .find(|e| is_opt_out(&e.record))
        .map(|e| e.end)
}

/// The same for a bot's journal on disk.
pub fn unshipped_opt_out(config_path: &Path) -> Option<u64> {
    let journal = files::read(&files::path(config_path, LogFile::Journal))
        .ok()
        .flatten()?;
    opt_out_end(&journal, shipped_offset(config_path))
}

/// One record in the journal and the offset just past its line.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub record: ChangeRecord,
    pub end: u64,
}

/// The next batch in a journal, read from byte `from`.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub entries: Vec<Entry>,
    /// Offset just past the last line consumed.
    pub end: u64,
    /// Complete lines that weren't a record. Only a crash between two
    /// appends can leave one (a torn line followed by a whole one), and
    /// re-sending it forever would stall every record after it, so it is
    /// consumed and counted instead.
    pub skipped: usize,
}

/// Pure: complete lines from `from`, up to `max` records. A trailing line
/// with no newline is still being written (or was torn) and stays pending.
/// A cursor past the end of the journal means the journal was replaced, and
/// starts over from zero; the API's dedupe makes that safe.
pub fn pending(journal: &[u8], from: u64, max: usize) -> Pending {
    let start = if from as usize > journal.len() {
        0
    } else {
        from as usize
    };
    let mut entries = Vec::new();
    let mut skipped = 0;
    let mut pos = start;
    while entries.len() < max {
        let Some(newline) = journal[pos..].iter().position(|b| *b == b'\n') else {
            break;
        };
        let line = &journal[pos..pos + newline];
        pos += newline + 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<ChangeRecord>(line) {
            Ok(record) => entries.push(Entry {
                record,
                end: pos as u64,
            }),
            Err(_) => skipped += 1,
        }
    }
    Pending {
        entries,
        end: pos as u64,
        skipped,
    }
}

/// What a batch is sent with besides its records.
#[derive(Debug, Clone)]
pub struct Shipment {
    pub endpoint: String,
    pub api_key: String,
    pub instance_id: Option<String>,
    pub wallet: Option<String>,
}

/// Why a POST didn't land.
#[derive(Debug)]
enum PostError {
    /// Textile read the body and said no (400, 413, 422). Sending the same
    /// bytes again gets the same answer.
    Refused(String),
    /// Anything else: unreachable, 5xx, a key it doesn't accept right now.
    /// Worth retrying later, and never a reason to skip a record.
    Failed(anyhow::Error),
}

/// Send everything pending, batch by batch, stopping at byte `until` when
/// given. Returns how many records Textile acknowledged.
///
/// A refused batch is split and sent one record at a time, so one record
/// the API won't take (a value past its size limits, say) is skipped with an
/// error instead of holding back everything after it. A skipped record stays
/// in the local journal. Any other failure stops here with the cursor on the
/// last record that landed.
pub async fn ship_pending(
    client: &reqwest::Client,
    config_path: &Path,
    shipment: &Shipment,
    until: Option<u64>,
) -> Result<usize> {
    let cursor = files::path(config_path, LogFile::Shipped);
    let mut shipped = 0;
    loop {
        let Some(journal) = files::read(&files::path(config_path, LogFile::Journal))
            .context("reading the change-log journal")?
        else {
            return Ok(shipped);
        };
        let end = until.map_or(journal.len(), |u| (u as usize).min(journal.len()));
        let from = shipped_offset(config_path);
        let batch = pending(&journal[..end], from, MAX_BATCH);
        if batch.skipped > 0 {
            tracing::error!(
                skipped = batch.skipped,
                "the change-log journal has lines that aren't records; skipping them"
            );
        }
        let records: Vec<ChangeRecord> = batch.entries.iter().map(|e| e.record.clone()).collect();
        if !records.is_empty() {
            match post(client, shipment, &records).await {
                Ok(()) => shipped += records.len(),
                Err(PostError::Refused(why)) => {
                    tracing::warn!(
                        "Textile refused a batch ({why}); sending it one record at a time"
                    );
                    shipped +=
                        ship_one_by_one(client, shipment, config_path, &batch.entries).await?;
                }
                Err(PostError::Failed(e)) => return Err(e),
            }
        }
        if batch.end != from {
            save_offset(&cursor, batch.end)?;
        }
        if batch.entries.len() < MAX_BATCH {
            return Ok(shipped);
        }
    }
}

async fn ship_one_by_one(
    client: &reqwest::Client,
    shipment: &Shipment,
    config_path: &Path,
    entries: &[Entry],
) -> Result<usize> {
    let cursor = files::path(config_path, LogFile::Shipped);
    let mut shipped = 0;
    for entry in entries {
        match post(client, shipment, std::slice::from_ref(&entry.record)).await {
            Ok(()) => shipped += 1,
            Err(PostError::Refused(why)) => {
                tracing::error!(
                    record = %entry.record.id,
                    parameter = %entry.record.parameter,
                    "Textile refused this change record ({why}); it stays in the local journal only"
                );
                // Noted before the cursor moves past it: the cursor says
                // "nothing left to send", and this one was never received.
                note_refused(config_path, &entry.record, &why)?;
            }
            Err(PostError::Failed(e)) => return Err(e),
        }
        save_offset(&cursor, entry.end)?;
    }
    Ok(shipped)
}

/// Add a record to `change-log.refused`. Rewritten whole through the safe
/// writer rather than appended, so whichever of the bot or a root panel
/// writes it, the other can still replace it.
fn note_refused(config_path: &Path, record: &ChangeRecord, why: &str) -> Result<()> {
    let path = files::path(config_path, LogFile::Refused);
    let existing = files::read_text(&path)?.unwrap_or_default();
    let line = serde_json::to_string(&json!({
        "id": record.id,
        "parameter": record.parameter,
        "changedAt": record.changed_at,
        "reason": why,
    }))?;
    crate::setup::write_file_atomic(&path, format!("{existing}{line}\n"))
        .context("noting a refused change record")
}

/// Records Textile refused and only this host has. They aren't owed (they
/// can't be delivered), but deleting the journal would lose them.
pub fn refused(config_path: &Path) -> usize {
    files::read_text(&files::path(config_path, LogFile::Refused))
        .ok()
        .flatten()
        .map_or(0, |text| {
            text.lines().filter(|l| !l.trim().is_empty()).count()
        })
}

fn save_offset(cursor: &Path, offset: u64) -> Result<()> {
    crate::setup::write_file_atomic(cursor, offset.to_string())
        .context("saving how far the change log has shipped")
}

async fn post(
    client: &reqwest::Client,
    shipment: &Shipment,
    records: &[ChangeRecord],
) -> std::result::Result<(), PostError> {
    let body = json!({
        "instanceId": shipment.instance_id,
        "wallet": shipment.wallet,
        "stitchVersion": env!("CARGO_PKG_VERSION"),
        "records": records,
    });
    let response = client
        .post(&shipment.endpoint)
        .bearer_auth(&shipment.api_key)
        .json(&body)
        .send()
        .await
        .context("sending change records to Textile")
        .map_err(PostError::Failed)?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let detail = response.text().await.unwrap_or_default();
    let why = format!("{status}: {}", detail.chars().take(300).collect::<String>());
    let refused = matches!(status.as_u16(), 400 | 413 | 422);
    Err(if refused {
        PostError::Refused(why)
    } else {
        PostError::Failed(anyhow::anyhow!(
            "Textile didn't take the change records ({why})"
        ))
    })
}

/// Records this bot owes Textile and hasn't delivered: everything unshipped
/// when its config ships, everything up to an unshipped switch-off when it
/// doesn't, and nothing for a bot that never opted in.
///
/// A config that can't be read or parsed (a broken hand edit) says nothing
/// either way, so the journal decides: a bot that has ever shipped or ever
/// had `ship = true` recorded owes everything unshipped. Failing open here
/// would let its only copy be deleted.
pub fn owed(config_path: &Path) -> usize {
    let Some(journal) = files::read(&files::path(config_path, LogFile::Journal))
        .ok()
        .flatten()
    else {
        return 0;
    };
    let from = shipped_offset(config_path);
    let end = match owing_mode(config_path, &journal, from) {
        Mode::Off => return 0,
        Mode::Continuous => journal.len(),
        Mode::FlushUntil(end) => end as usize,
    };
    pending(&journal[..end], from, usize::MAX).entries.len()
}

/// How this bot would ship right now, judged from disk.
fn owing_mode(config_path: &Path, journal: &[u8], from: u64) -> Mode {
    let ships_now = files::read_text(config_path)
        .ok()
        .flatten()
        .and_then(|t| toml::from_str::<Config>(&t).ok())
        .map(|cfg| cfg.change_log.ship);
    match ships_now {
        Some(ships) => mode(ships, opt_out_end(journal, from)),
        None if ever_opted_in(config_path, journal) => Mode::Continuous,
        None => Mode::Off,
    }
}

fn ever_opted_in(config_path: &Path, journal: &[u8]) -> bool {
    files::path(config_path, LogFile::Shipped).exists()
        || pending(journal, 0, usize::MAX).entries.iter().any(|e| {
            e.record.parameter == "change_log.ship" && e.record.after.as_deref() == Some("true")
        })
}

/// Deliver what this bot owes Textile with the maker key it has now.
///
/// Call this before the key changes (a new Connect, a vault switch, a pasted
/// key). The API files a record under whichever maker's key ships it, so
/// anything still queued when the key changes would be filed under the new
/// maker. Errors when something owed couldn't be delivered: the caller
/// should refuse the key change rather than make it anyway. Returns how many
/// records went out.
pub async fn deliver_owed(config_path: &Path) -> Result<usize> {
    if owed(config_path) == 0 {
        return Ok(0);
    }
    let text = files::read_text(config_path)?
        .context("the config is gone, so there's no key to deliver with")?;
    let cfg = toml::from_str::<Config>(&text)
        .context("the config doesn't parse, so there's no key to deliver with")?;
    let journal = files::read(&files::path(config_path, LogFile::Journal))?.unwrap_or_default();
    let until = match owing_mode(config_path, &journal, shipped_offset(config_path)) {
        Mode::FlushUntil(end) => Some(end),
        _ => None,
    };
    let shipment = shipment_for(&cfg, config_path, None)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let delivered = ship_pending(&client, config_path, &shipment, until).await?;
    let left = owed(config_path);
    anyhow::ensure!(left == 0, "{left} record(s) still undelivered");
    Ok(delivered)
}

fn shipped_offset(config_path: &Path) -> u64 {
    files::read_text(&files::path(config_path, LogFile::Shipped))
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Where and as whom this bot ships, or why it can't.
pub fn shipment_for(cfg: &Config, config_path: &Path, wallet: Option<String>) -> Result<Shipment> {
    let rfq = cfg.rfq.as_ref().context(
        "[change_log] ship is on, but this bot has no [rfq] block, so it has no Textile \
         credential to ship with. Connect it to Textile first.",
    )?;
    let dir = config_path.parent().filter(|p| !p.as_os_str().is_empty());
    let api_key = crate::rfq::load_rfq_api_key(&rfq.api_key_env, dir)
        .context("[change_log] ship is on, but the maker API key can't be read")?;
    let origin = crate::venue::enroll::venue_origin_from_config(cfg, None);
    let endpoint = format!("{}{ENDPOINT_PATH}", origin.trim_end_matches('/'));
    crate::config::assert_feed_url(&endpoint, "change log endpoint")?;
    Ok(Shipment {
        endpoint,
        api_key,
        instance_id: crate::rfq::resolve_instance_id(rfq.instance_id.as_deref(), dir),
        wallet,
    })
}

/// Ship in the background for as long as `mode` says. `None` when it says
/// not to, or when shipping is on but can't work (logged loudly; records
/// stay in the journal and ship once it can).
pub fn spawn(
    cfg: &Config,
    config_path: &Path,
    wallet: Option<String>,
    mode: Mode,
) -> Option<tokio::task::JoinHandle<()>> {
    if mode == Mode::Off {
        return None;
    }
    let shipment = match shipment_for(cfg, config_path, wallet) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("parameter changes are recorded locally but can't ship: {e:#}");
            return None;
        }
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let config_path = config_path.to_owned();
    let until = match mode {
        Mode::FlushUntil(end) => Some(end),
        _ => None,
    };
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match ship_pending(&client, &config_path, &shipment, until).await {
                Ok(n) => {
                    if n > 0 {
                        tracing::info!(records = n, "shipped parameter changes to Textile");
                    }
                    if until.is_some() {
                        tracing::info!("[change_log] ship is off now; stopped shipping");
                        return;
                    }
                }
                Err(e) => tracing::warn!("parameter changes didn't ship, will retry: {e:#}"),
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changelog::journal::{record_startup, write_config_recorded};
    use crate::changelog::record::{random_uuid, Source};

    const CONFIG: &str = include_str!("testdata/zaru.toml");

    fn shipping() -> String {
        format!("{CONFIG}\n[change_log]\nship = true\n")
    }

    fn line(parameter: &str) -> String {
        let record = ChangeRecord {
            id: random_uuid(),
            change_set_id: "set".into(),
            changed_at: "2026-10-22T10:00:00.000Z".into(),
            detected_at: None,
            source: "panel.settings".into(),
            actor: "tailnet:ops@textile.example".into(),
            bot: None,
            chain_id: 8453,
            scope: "bot".into(),
            scope_label: None,
            parameter: parameter.into(),
            unit: "bps".into(),
            before: Some("1".into()),
            after: Some("2".into()),
            config_sha256_before: None,
            config_sha256_after: None,
        };
        serde_json::to_string(&record).unwrap() + "\n"
    }

    #[test]
    fn mode_ships_up_to_the_switch_off_and_then_stops() {
        assert_eq!(mode(true, None), Mode::Continuous);
        assert_eq!(mode(true, Some(9)), Mode::Continuous);
        assert_eq!(mode(false, Some(9)), Mode::FlushUntil(9));
        assert_eq!(mode(false, None), Mode::Off);
    }

    fn switch(before: &str, after: &str) -> String {
        line("change_log.ship")
            .replace("\"before\":\"1\"", &format!("\"before\":\"{before}\""))
            .replace("\"after\":\"2\"", &format!("\"after\":\"{after}\""))
    }

    #[test]
    fn the_newest_unshipped_switch_off_is_where_a_flush_stops() {
        let off = switch("true", "false");
        let journal = format!("{}{off}{}", line("a"), line("b"));
        let end = (line("a").len() + off.len()) as u64;
        assert_eq!(opt_out_end(journal.as_bytes(), 0), Some(end));
        // Already shipped past it: nothing owed.
        assert_eq!(opt_out_end(journal.as_bytes(), end), None);
        // Switching on is not a switch-off.
        let on = format!("{}{}", line("a"), switch("false", "true"));
        assert_eq!(opt_out_end(on.as_bytes(), 0), None);
    }

    #[test]
    fn pending_reads_complete_lines_from_the_cursor() {
        let journal = format!("{}{}{}", line("a"), line("b"), line("c"));
        let first = pending(journal.as_bytes(), 0, 2);
        assert_eq!(first.entries.len(), 2);
        assert_eq!(first.entries[1].record.parameter, "b");
        assert_eq!(first.entries[0].end as usize, line("a").len());
        let rest = pending(journal.as_bytes(), first.end, 2);
        assert_eq!(rest.entries.len(), 1);
        assert_eq!(rest.entries[0].record.parameter, "c");
        assert_eq!(rest.end as usize, journal.len());
        assert!(pending(journal.as_bytes(), rest.end, 2).entries.is_empty());
    }

    #[test]
    fn a_line_still_being_written_waits() {
        let whole = line("a");
        let torn = &line("b")[..20];
        let journal = format!("{whole}{torn}");
        let p = pending(journal.as_bytes(), 0, 10);
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.end as usize, whole.len());
    }

    #[test]
    fn a_torn_line_followed_by_a_whole_one_is_skipped_not_stuck() {
        let journal = format!("{{\"id\":\"tor\n{}", line("b"));
        let p = pending(journal.as_bytes(), 0, 10);
        assert_eq!(p.skipped, 1);
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.end as usize, journal.len());
    }

    #[test]
    fn a_cursor_past_the_end_starts_over() {
        let journal = line("a");
        let p = pending(journal.as_bytes(), 10_000, 10);
        assert_eq!(p.entries.len(), 1);
    }

    fn temp_config(tag: &str, text: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stitch-ship-{}-{tag}-{}",
            std::process::id(),
            random_uuid()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stitch.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A stand-in for the API that stores bodies and answers `status`.
    async fn mock_api(
        status: axum::http::StatusCode,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let store = seen.clone();
        let app = axum::Router::new().route(
            ENDPOINT_PATH,
            axum::routing::post(
                move |headers: axum::http::HeaderMap,
                      axum::Json(body): axum::Json<serde_json::Value>| {
                    let store = store.clone();
                    async move {
                        assert_eq!(
                            headers.get("authorization").and_then(|v| v.to_str().ok()),
                            Some("Bearer tx_live_test.secret")
                        );
                        store.lock().unwrap().push(body);
                        (status, axum::Json(json!({ "data": { "accepted": 1 } })))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        (origin, seen)
    }

    fn shipment(origin: &str) -> Shipment {
        Shipment {
            endpoint: format!("{origin}{ENDPOINT_PATH}"),
            api_key: "tx_live_test.secret".into(),
            instance_id: Some("stitch-acme".into()),
            wallet: Some("0xabc".into()),
        }
    }

    #[tokio::test]
    async fn shipping_sends_each_record_once_and_remembers_how_far_it_got() {
        let (origin, seen) = mock_api(axum::http::StatusCode::OK).await;
        let path = temp_config("ok", &shipping());
        record_startup(&path, &shipping(), 1).unwrap();
        let client = reqwest::Client::new();
        let baseline = ship_pending(&client, &path, &shipment(&origin), None)
            .await
            .unwrap();
        assert!(baseline > 0);

        let next = shipping().replace("buy_offset_bps = 40", "buy_offset_bps = 35");
        let origin_of = crate::changelog::journal::panel_origin(
            Source::PanelSettings,
            "tailnet:ops@textile.example",
            "acme",
        );
        write_config_recorded(&path, &next, &origin_of, 2, None).unwrap();
        assert_eq!(owed(&path), 1);
        assert_eq!(
            ship_pending(&client, &path, &shipment(&origin), None)
                .await
                .unwrap(),
            1
        );
        assert_eq!(owed(&path), 0);
        assert_eq!(
            ship_pending(&client, &path, &shipment(&origin), None)
                .await
                .unwrap(),
            0
        );

        let bodies = seen.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[1]["instanceId"], "stitch-acme");
        assert_eq!(bodies[1]["wallet"], "0xabc");
        let record = &bodies[1]["records"][0];
        assert_eq!(record["parameter"], "buy_offset_bps");
        assert_eq!(record["before"], "40");
        assert_eq!(record["after"], "35");
        assert_eq!(record["actor"], "tailnet:ops@textile.example");
    }

    #[tokio::test]
    async fn a_batch_that_does_not_land_stays_pending() {
        let (origin, seen) = mock_api(axum::http::StatusCode::SERVICE_UNAVAILABLE).await;
        let path = temp_config("refused", CONFIG);
        record_startup(&path, CONFIG, 1).unwrap();
        let client = reqwest::Client::new();
        assert!(ship_pending(&client, &path, &shipment(&origin), None)
            .await
            .is_err());
        assert!(ship_pending(&client, &path, &shipment(&origin), None)
            .await
            .is_err());
        // Same batch both times: nothing was marked shipped.
        let bodies = seen.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["records"], bodies[1]["records"]);
    }

    /// An API that refuses any batch holding a record for `poison`, the way
    /// zod refuses a value past its limits, and takes everything else.
    async fn picky_api() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::response::IntoResponse;
        let stored = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let store = stored.clone();
        let app = axum::Router::new().route(
            ENDPOINT_PATH,
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let store = store.clone();
                async move {
                    let records = body["records"].as_array().cloned().unwrap_or_default();
                    if records.iter().any(|r| r["parameter"] == "poison") {
                        return (axum::http::StatusCode::BAD_REQUEST, "too long").into_response();
                    }
                    store.lock().unwrap().extend(
                        records
                            .iter()
                            .map(|r| r["parameter"].as_str().unwrap().to_string()),
                    );
                    axum::Json(json!({ "data": {} })).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        (origin, stored)
    }

    #[tokio::test]
    async fn one_record_the_api_refuses_is_skipped_not_a_wall() {
        let (origin, stored) = picky_api().await;
        let path = temp_config("poison", &shipping());
        let journal_path = files::path(&path, LogFile::Journal);
        let journal = format!("{}{}{}", line("a"), line("poison"), line("c"));
        std::fs::write(&journal_path, &journal).unwrap();

        let client = reqwest::Client::new();
        assert_eq!(
            ship_pending(&client, &path, &shipment(&origin), None)
                .await
                .unwrap(),
            2
        );
        assert_eq!(*stored.lock().unwrap(), ["a", "c"]);
        assert_eq!(owed(&path), 0, "the cursor moved past the refused record");
        // The refused record is still in the local journal.
        assert!(std::fs::read_to_string(&journal_path)
            .unwrap()
            .contains("poison"));
        assert_eq!(
            ship_pending(&client, &path, &shipment(&origin), None)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_panel_switch_off_still_ships_and_nothing_after_it_does() {
        let (origin, seen) = mock_api(axum::http::StatusCode::OK).await;
        let on = shipping();
        let path = temp_config("optout", &on);
        record_startup(&path, &on, 1).unwrap();
        let client = reqwest::Client::new();
        ship_pending(&client, &path, &shipment(&origin), None)
            .await
            .unwrap();

        // The operator switches shipping off from the panel, then saves a
        // spread. Both land before the bot restarts.
        let panel = |source| {
            crate::changelog::journal::panel_origin(source, "tailnet:ops@textile.example", "acme")
        };
        let off = on.replace("ship = true", "ship = false");
        write_config_recorded(&path, &off, &panel(Source::PanelRaw), 2, None).unwrap();
        let later = off.replace("buy_offset_bps = 40", "buy_offset_bps = 35");
        write_config_recorded(&path, &later, &panel(Source::PanelSettings), 3, None).unwrap();

        // The panel already moved the snapshot, so the restart finds nothing
        // new, and the switch-off is still owed: deleting the bot is refused.
        assert_eq!(record_startup(&path, &later, 4).unwrap(), 0);
        assert_eq!(owed(&path), 1);
        let cfg = Config::from_toml(&later).unwrap();
        let Mode::FlushUntil(end) = mode(cfg.change_log.ship, unshipped_opt_out(&path)) else {
            panic!("a switch-off nobody received must be flushed");
        };

        assert_eq!(
            ship_pending(&client, &path, &shipment(&origin), Some(end))
                .await
                .unwrap(),
            1
        );
        let bodies = seen.lock().unwrap();
        let last = &bodies.last().unwrap()["records"];
        assert_eq!(last.as_array().unwrap().len(), 1);
        assert_eq!(last[0]["parameter"], "change_log.ship");
        assert_eq!(last[0]["after"], "false");
        // Delivered: nothing owed, and the spread saved with shipping off
        // stays local.
        assert_eq!(owed(&path), 0);
        assert_eq!(mode(false, unshipped_opt_out(&path)), Mode::Off);
    }

    #[test]
    fn a_broken_config_on_a_bot_that_shipped_still_owes_its_records() {
        let path = temp_config("broken", &shipping());
        record_startup(&path, &shipping(), 1).unwrap();
        let queued = owed(&path);
        assert!(queued > 0);
        std::fs::write(&path, "chain_id = ").unwrap();
        assert_eq!(owed(&path), queued, "unreadable is not an opt-out");

        // A bot that never opted in owes nothing, broken or not.
        let never = temp_config("never", CONFIG);
        record_startup(&never, CONFIG, 1).unwrap();
        std::fs::write(&never, "chain_id = ").unwrap();
        assert_eq!(owed(&never), 0);
    }

    /// The shipping config with an `[rfq]` block pointing at `origin`, and a
    /// maker key beside it.
    fn connected(tag: &str, origin: &str) -> std::path::PathBuf {
        let rfq = format!(
            "\n[rfq]\nenabled = true\nurl = \"{}/v2/maker/stream\"\nmaker_id = \"m\"\n\
             validation_contract = \"0x00000000000000000000000000000000000000cc\"\n\
             instance_id = \"stitch-acme\"\n",
            origin.replace("http://", "ws://")
        );
        let text = format!("{}{rfq}", shipping());
        let path = temp_config(tag, &text);
        crate::setup::write_rfq_api_key(path.parent().unwrap(), "tx_live_test.secret").unwrap();
        record_startup(&path, &text, 1).unwrap();
        path
    }

    #[tokio::test]
    async fn owed_records_go_out_under_the_current_key_before_it_changes() {
        let (origin, seen) = mock_api(axum::http::StatusCode::OK).await;
        let path = connected("deliver", &origin);
        assert!(owed(&path) > 0);
        let delivered = deliver_owed(&path).await.unwrap();
        assert!(delivered > 0);
        assert_eq!(owed(&path), 0);
        assert_eq!(seen.lock().unwrap().len(), 1);
        // Nothing owed: no call at all.
        assert_eq!(deliver_owed(&path).await.unwrap(), 0);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_undeliverable_queue_blocks_the_key_change() {
        let (origin, _seen) = mock_api(axum::http::StatusCode::SERVICE_UNAVAILABLE).await;
        let path = connected("blocked", &origin);
        assert!(deliver_owed(&path).await.is_err());
        assert!(owed(&path) > 0);
    }

    #[tokio::test]
    async fn a_refused_record_is_noted_so_deleting_the_journal_stays_blocked() {
        let (origin, _stored) = picky_api().await;
        let path = temp_config("refused", &shipping());
        let journal = format!("{}{}", line("a"), line("poison"));
        std::fs::write(files::path(&path, LogFile::Journal), &journal).unwrap();
        let client = reqwest::Client::new();
        ship_pending(&client, &path, &shipment(&origin), None)
            .await
            .unwrap();
        assert_eq!(owed(&path), 0, "nothing left to send");
        assert_eq!(refused(&path), 1, "but one record only this host has");
        let noted = std::fs::read_to_string(files::path(&path, LogFile::Refused)).unwrap();
        assert!(noted.contains("\"parameter\":\"poison\""), "{noted}");
    }

    #[test]
    fn shipping_needs_a_textile_credential() {
        let path = temp_config("nokey", CONFIG);
        let cfg = Config::from_toml(CONFIG).unwrap();
        let err = shipment_for(&cfg, &path, None).unwrap_err();
        assert!(format!("{err:#}").contains("no [rfq] block"), "{err:#}");
    }
}

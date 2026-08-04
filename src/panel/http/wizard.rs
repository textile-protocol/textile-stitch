// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Adding a bot.
//!
//! The corridor list and the file writing both come from [`crate::setup`], the
//! same code the desktop wizard runs, so a config created here is byte-identical
//! to one created there. This module only translates JSON into a
//! [`setup::SignerSetup`] and then creates the container.
//!
//! Secrets are write-only. They arrive in the request body, go through the writer
//! into an owner-only file, and are never read back by any route.

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::{bots, ApiError, AppState};
use crate::panel::{naming, provision};
use crate::setup::{self, LocalKeyMaterial, SignerSetup};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CorridorBody {
    pub id: &'static str,
    pub display_name: &'static str,
    pub network_label: &'static str,
    pub chain_id: u64,
    /// The `stitch.toml` this corridor ships, so the wizard can show exactly what
    /// it is about to write.
    pub toml_template: &'static str,
}

/// The corridors a new bot can be created for, in display order.
pub async fn corridors() -> Response {
    let list: Vec<_> = setup::catalog()
        .iter()
        .map(|c| CorridorBody {
            id: c.id,
            display_name: c.display_name,
            network_label: c.network_label,
            chain_id: c.chain_id,
            toml_template: c.toml_template,
        })
        .collect();
    Json(serde_json::json!({ "corridors": list })).into_response()
}

/// Generate a fresh hot wallet for the "Create wallet" step.
///
/// Returns the seed phrase once so the SPA can show a backup screen. The phrase
/// is not stored server-side — the client posts it back (as `seedPhrase`) on
/// create / change-signer, and the writer persists only the derived hex key.
pub async fn generate_wallet() -> Result<Response, ApiError> {
    let wallet = crate::signer::generate_local_wallet()?;
    Ok(Json(serde_json::json!({
        "address": format!("{:?}", wallet.address).to_lowercase(),
        "seedPhrase": wallet.seed_phrase,
    }))
    .into_response())
}

/// The signer half of the wizard payload.
///
/// Tagged on `kind` so the shape and the backend can't disagree, and so a missing
/// field for the chosen backend is a deserialization error with the field name in
/// it rather than a silently empty string.
#[derive(Deserialize)]
// `rename_all` picks the variant tags (`local`, `turnkey`, `mpcvault`);
// `rename_all_fields` picks the field names inside every variant. Both are needed:
// without the second, `privateKey` silently doesn't bind and a valid request looks
// like a missing key.
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SignerRequest {
    /// Hot wallet. Exactly one of the two key forms.
    Local {
        #[serde(default)]
        private_key: Option<String>,
        #[serde(default)]
        seed_phrase: Option<String>,
    },
    Turnkey {
        organization_id: String,
        sign_with: String,
        operator_address: String,
        #[serde(default)]
        api_base_url: Option<String>,
        api_public_key: String,
        api_private_key: String,
    },
    Mpcvault {
        vault_uuid: String,
        client_signer_pubkey: String,
        operator_address: String,
        #[serde(default)]
        api_base_url: Option<String>,
        #[serde(default)]
        callback_listen_addr: Option<String>,
        api_token: String,
    },
}

impl SignerRequest {
    /// The validated backend + secret material. Shared with the signer-change route.
    pub(crate) fn into_setup(self) -> Result<SignerSetup, ApiError> {
        match self {
            SignerRequest::Local {
                private_key,
                seed_phrase,
            } => {
                let key = private_key.filter(|s| !s.trim().is_empty());
                let phrase = seed_phrase.filter(|s| !s.trim().is_empty());
                let material = match (key, phrase) {
                    (Some(_), Some(_)) => {
                        return Err(ApiError::bad_request(
                            "send either a private key or a seed phrase, not both",
                        ))
                    }
                    (Some(k), None) => LocalKeyMaterial::PrivateKey(k),
                    (None, Some(p)) => LocalKeyMaterial::SeedPhrase(p),
                    (None, None) => {
                        return Err(ApiError::bad_request(
                            "a hot wallet needs a private key or a seed phrase",
                        ))
                    }
                };
                Ok(SignerSetup::Local { material })
            }
            SignerRequest::Turnkey {
                organization_id,
                sign_with,
                operator_address,
                api_base_url,
                api_public_key,
                api_private_key,
            } => Ok(SignerSetup::Turnkey {
                organization_id,
                sign_with,
                operator_address,
                api_base_url: blank_to_none(api_base_url),
                api_public_key,
                api_private_key,
            }),
            SignerRequest::Mpcvault {
                vault_uuid,
                client_signer_pubkey,
                operator_address,
                api_base_url,
                callback_listen_addr,
                api_token,
            } => Ok(SignerSetup::Mpcvault {
                vault_uuid,
                client_signer_pubkey,
                operator_address,
                api_base_url: blank_to_none(api_base_url),
                callback_listen_addr: blank_to_none(callback_listen_addr),
                api_token,
            }),
        }
    }
}

fn blank_to_none(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// The operator address a signer selects, lowercased the way discovery formats it.
fn operator_address_of(setup: &SignerSetup) -> Result<String, ApiError> {
    match setup {
        SignerSetup::Local { material } => {
            let addr = material.operator_address().map_err(ApiError::bad_request)?;
            Ok(format!("{addr:?}").to_lowercase())
        }
        SignerSetup::Turnkey {
            operator_address, ..
        }
        | SignerSetup::Mpcvault {
            operator_address, ..
        } => Ok(operator_address.trim().to_lowercase()),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignerCheckRequest {
    /// Chain the new bot (or the bot being re-signed) will trade on.
    pub chain_id: u64,
    pub signer: SignerRequest,
    /// When re-signing an existing bot, exclude it — it already uses this wallet.
    #[serde(default)]
    pub exclude_bot: Option<String>,
}

/// Dry-run: which other bots in the fleet already use this wallet on this chain.
///
/// Sharing one operator wallet across two bots on the same chain is unsafe (nonce
/// collisions, competing quotes). The UI warns before create / change-signer.
/// Config writes are still allowed when the conflict is soft, but a *live*
/// sibling with taker/closer on (`blocksLiveSwitch`) makes change-signer /
/// Start refuse — the response marks those so the UI doesn't offer a confirm
/// that can only 409.
pub async fn check_signer(
    State(state): State<AppState>,
    Json(body): Json<SignerCheckRequest>,
) -> Result<Response, ApiError> {
    let setup = body.signer.into_setup()?;
    let address = operator_address_of(&setup)?;
    let wallet = crate::panel::inventory::WalletId {
        chain_id: body.chain_id,
        address: address.clone(),
    };
    let exclude = body
        .exclude_bot
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let fleet = state.fleet().await?;
    let conflicts: Vec<serde_json::Value> = fleet
        .bots()
        .iter()
        .filter(|b| exclude != Some(b.name.as_str()))
        .filter(|b| b.wallet().as_ref() == Some(&wallet))
        .map(|b| {
            // Same predicate change_signer / Start use via no_live_sibling_on_wallet_id.
            let blocks_live_switch = super::logs::already_transacting(b);
            serde_json::json!({
                "name": b.name,
                "chainId": b.config.as_ref().map(|c| c.chain_id),
                "operatorAddress": b.config.as_ref().and_then(|c| c.operator_address.clone()),
                "blocksLiveSwitch": blocks_live_switch,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "operatorAddress": address,
        "chainId": body.chain_id,
        "conflicts": conflicts,
    }))
    .into_response())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRequest {
    /// Bot name. Becomes the config directory and part of the container name.
    pub name: String,
    pub corridor_id: String,
    pub signer: SignerRequest,
    /// Start the bot immediately. Off by default: the recommended path is to
    /// approve Permit2 (costs a little gas) and dry-run first.
    #[serde(default)]
    pub start: bool,
}

/// Create a bot: write its config, then create its container.
///
/// Ordering matters. The name is checked against the live fleet before anything is
/// written, and the config is written before the container is created — so a
/// failure at the Docker step leaves a valid config the operator can retry from,
/// not a container pointing at files that don't exist.
pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateRequest>,
) -> Result<Response, ApiError> {
    let name = body.name.trim().to_string();
    naming::validate_bot_id(&name).map_err(ApiError::bad_request)?;

    let corridor = setup::find_corridor(&body.corridor_id).ok_or_else(|| {
        ApiError::bad_request(format!(
            "there is no corridor called \"{}\". Ask /api/corridors for the list.",
            body.corridor_id
        ))
    })?;

    let fleet = state.fleet().await?;
    if fleet.contains(&name) {
        return Err(ApiError::conflict(format!(
            "there is already a bot called \"{name}\". Pick another name, or remove that one \
             first."
        )));
    }
    // Claim the directory by creating it, and treat "it already exists" as the
    // refusal. `create_dir` is atomic, which is the point: the fleet snapshot above
    // and a `has_operator_files` probe are both reads, so two requests for the same
    // name could each pass their own checks and then write through the same temp
    // filenames, one clobbering the other's config or key. Whoever gets the mkdir
    // owns the name; the loser is told, before either has written a byte.
    //
    // It also means the panel only ever hands the bot a directory it created, so the
    // handover below can't reach an operator's README, backup or recovered ledger —
    // there is nothing else in there.
    // Validated before the directory is claimed, not after. This is pure request-body
    // checking with no filesystem in it, and every early return between the claim and
    // the writer's cleanup would leave an empty directory behind that makes the
    // corrected retry fail with `AlreadyExists` until someone rmdir's it by hand.
    let signer = body.signer.into_setup()?;

    let dir = state.cfg.bot_dir(&name);
    if let Err(e) = std::fs::create_dir(&dir) {
        return Err(match e.kind() {
            std::io::ErrorKind::AlreadyExists => ApiError::conflict(format!(
                "{} already exists. The wizard only writes into a directory it creates, so move \
                 that one aside or pick another name.",
                dir.display()
            )),
            _ => ApiError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("creating {}: {e}", dir.display()),
            ),
        });
    }

    // The writer validates every secret before it touches the filesystem, so a
    // bad key fails here with its own message rather than half a config on disk.
    // Surfaced verbatim: it is the most useful thing we can say.
    //
    // The directory goes with it on failure. We created it, so nothing else can be in
    // there — and leaving an empty one behind would make the `create_dir` above refuse
    // every retry of the name the operator just got wrong.
    if let Err(e) = setup::write_config_signer(&dir, corridor, &signer) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(ApiError::bad_request(format!("{e:#}")));
    }

    // The panel writes as root; the bot runs as `stitch` and can't even chmod its
    // own run directory unless it owns these files. Without this every bot the
    // wizard creates exits on startup.
    provision::hand_over_to_bot(&dir, state.cfg.bot_uid).map_err(|e| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("{e:#}"),
        )
    })?;

    let runtime = provision::signer_runtime(&dir)?;
    let spec = provision::bot_container_spec(
        &state.cfg,
        &name,
        &state.cfg.bot_image,
        &runtime,
        Some(corridor.id),
    );
    // Docker's create endpoint doesn't pull, so on a host that has never run a
    // bot this is what actually fetches the image. Both failures land on the same
    // message: the config is on disk, so Recreate is the way back in.
    // Before the daemon is asked to mount anything: a source file that isn't there
    // gets created as a directory rather than refused, which would put a folder where
    // the config this wizard just wrote is supposed to be.
    provision::check_file_mounts(&spec.binds, &state.cfg).map_err(ApiError::conflict)?;
    let created = match state.docker.ensure_image(&spec.image, false).await {
        Ok(()) => state.docker.create(&spec).await,
        Err(e) => Err(e),
    };
    created.map_err(|e| {
        ApiError::new(
            axum::http::StatusCode::BAD_GATEWAY,
            format!(
                "{name}'s config was written to {}, but creating its container failed: {e:#}. \
                 Fix the cause and use Recreate — the config is already there.",
                dir.display()
            ),
        )
    })?;

    // Starting a brand-new bot is a launch like any other, and its signer is one the
    // caller just supplied — quite possibly the same key an approval is running
    // against, or that another bot already quotes with. So it goes through the same
    // config-lock / read / claim / start sequence as the Start handler, not a bare
    // `docker.start`: a concurrent raw save could move the wallet between the read and
    // the start, leaving the container signing from a wallet nothing claimed.
    let mut started = false;
    let mut start_error = None;
    if body.start {
        let (_config, bot) = bots::lock_config(&name, &state).await?;
        match bots::claim_for_launch(&bot, &state).await {
            Ok(wallet) => match state.docker.start(&spec.name).await {
                Ok(()) => started = true,
                // The start can fail after the daemon already brought the container up on
                // the claimed wallet; settle it (stop, then release the claim, or hold it
                // until the stop lands) so a sibling can't launch on a live-but-unclaimed
                // wallet — the same guard the Start handler uses.
                Err(e) => {
                    let settled = bots::settle_ambiguous_launch(
                        &state,
                        &spec.name,
                        wallet,
                        ApiError::internal(&e),
                    )
                    .await;
                    start_error = Some(settled.message.clone());
                }
            },
            // The bot exists and its config is right; only the start is refused. Told
            // rather than failed, because undoing the create would be worse than
            // leaving a stopped bot the operator can start in a moment.
            Err(e) => start_error = Some(e.message.clone()),
        }
        // `_config` and the claim are held across `docker.start`, then dropped here.
    }
    if let Some(reason) = &start_error {
        tracing::warn!(bot = %name, "created but not started: {reason}");
    }
    tracing::info!(bot = %name, corridor = %corridor.id, started, "created");

    // Re-read for the response — the wallet comes from the config just written, and the
    // state changed if the start took.
    let (bot, fleet) = state.bot_and_fleet(&name).await?;
    // Create never checks on-chain Permit2 allowances. `docker.start` succeeding
    // only means the container launched — the bot's own preflight can still fail
    // and restart-loop if approvals are missing. Always tell the UI to surface
    // the Permit2 handoff; Approve is a no-op when allowances are already set.
    let needs_permit2_approval = true;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(serde_json::json!({
            "bot": bots::to_body(&bot, &state, &fleet),
            "needsPermit2Approval": needs_permit2_approval,
            "message": format!(
                "{name} is set up for {} on {}. {}",
                corridor.display_name,
                corridor.network_label,
                match (&start_error, started) {
                    (Some(reason), _) => format!(
                        "It was created but not started: {reason} Start it from its page once that \
                         clears. Approve Permit2 first if that was the blocker (needs a little gas)."
                    ),
                    (None, true) => {
                        "It's running — check its logs. If it restart-loops, approve Permit2 \
                         (Tools → Approve allowances — needs a little gas)."
                            .to_string()
                    },
                    (None, false) => {
                        "Next: approve Permit2 for its input tokens (Tools → Approve \
                         allowances — needs a little gas), then dry-run before starting."
                            .to_string()
                    }
                }
            ),
        })),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{harness, Harness, TEST_KEY};
    use crate::panel::docker::fake::Call;
    use axum::http::StatusCode;
    use serde_json::json;

    fn local(key: &str) -> serde_json::Value {
        json!({ "kind": "local", "privateKey": key })
    }

    #[tokio::test]
    async fn the_corridor_list_carries_its_templates() {
        let h = harness("corridors");
        let (status, body) = h.get("/api/corridors").await;
        assert_eq!(status, StatusCode::OK);
        let v = Harness::parse(&body);
        let list = v["corridors"].as_array().unwrap();
        assert!(list.len() >= 7, "the shipped catalog");
        assert_eq!(list[0]["id"], "cngn-usdt-bsc");
        assert!(list[0]["tomlTemplate"]
            .as_str()
            .unwrap()
            .contains("[[pools]]"));
    }

    #[tokio::test]
    async fn generate_wallet_returns_a_phrase_and_matching_address() {
        let h = harness("gen-wallet");
        let (status, body) = h.post_json("/api/wallets/generate", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        let phrase = v["seedPhrase"].as_str().expect("seedPhrase");
        assert_eq!(phrase.split_whitespace().count(), 12);
        let address = v["address"].as_str().expect("address");
        assert!(address.starts_with("0x"));
        assert_eq!(address.len(), 42);
        // Round-trip through create so the derived key is what the fleet sees.
        let (status, _) = h
            .post_json(
                "/api/bots",
                json!({
                    "name": "fresh",
                    "corridorId": "cngn-usdt-bsc",
                    "signer": { "kind": "local", "seedPhrase": phrase },
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "create with generated phrase");
        let (_, show) = h.get("/api/bots/fresh").await;
        let shown = Harness::parse(&show);
        let shown_addr = shown["config"]["operatorAddress"]
            .as_str()
            .or_else(|| shown["operatorAddress"].as_str())
            .expect("operator address on bot");
        assert_eq!(shown_addr.to_lowercase(), address.to_lowercase());
        // Secrets stay write-only — the generate response is the only place the
        // phrase appears, and the bot detail never echoes key material.
        assert!(!show.contains(phrase));
    }

    #[tokio::test]
    async fn signer_check_warns_when_another_bot_shares_the_wallet_on_the_chain() {
        let h = harness("signer-check");
        // Existing bot on BSC with TEST_KEY.
        h.post_json(
            "/api/bots",
            json!({
                "name": "bot-a",
                "corridorId": "cngn-usdt-bsc",
                "signer": local(TEST_KEY),
            }),
        )
        .await;

        // Same key + same chain → conflict.
        let (status, body) = h
            .post_json(
                "/api/signer/check",
                json!({
                    "chainId": 56,
                    "signer": local(TEST_KEY),
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["conflicts"].as_array().unwrap().len(), 1);
        assert_eq!(v["conflicts"][0]["name"], "bot-a");
        // Fresh create is stopped / maker-only — soft conflict, not a live-switch block.
        assert_eq!(v["conflicts"][0]["blocksLiveSwitch"], false);
        assert!(!body.contains(TEST_KEY));

        // Same key on a different chain is fine.
        let (status, body) = h
            .post_json(
                "/api/signer/check",
                json!({
                    "chainId": 1,
                    "signer": local(TEST_KEY),
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(Harness::parse(&body)["conflicts"]
            .as_array()
            .unwrap()
            .is_empty());

        // Re-signing bot-a itself excludes it.
        let (status, body) = h
            .post_json(
                "/api/signer/check",
                json!({
                    "chainId": 56,
                    "signer": local(TEST_KEY),
                    "excludeBot": "bot-a",
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(Harness::parse(&body)["conflicts"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn signer_check_marks_a_live_transacting_sibling_as_blocking() {
        // change_signer refuses a restart onto a wallet a live taker already spends.
        // The check has to surface that so the UI doesn't offer "Switch anyway".
        let h = harness("signer-check-block");
        h.post_json(
            "/api/bots",
            json!({
                "name": "bot-a",
                "corridorId": "cngn-usdt-bsc",
                "signer": local(TEST_KEY),
                "start": true,
            }),
        )
        .await;
        // Turn the taker on and keep it running — that's what can_transact keys on.
        let path = h.root.join("bot-a/stitch.toml");
        let toml = std::fs::read_to_string(&path).unwrap() + "\nlimit_taker_enabled = true\n";
        std::fs::write(&path, toml).unwrap();

        let (status, body) = h
            .post_json(
                "/api/signer/check",
                json!({
                    "chainId": 56,
                    "signer": local(TEST_KEY),
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["conflicts"].as_array().unwrap().len(), 1);
        assert_eq!(v["conflicts"][0]["blocksLiveSwitch"], true, "{body}");
    }

    #[tokio::test]
    async fn creating_a_bot_writes_the_config_and_the_container() {
        let h = harness("create");
        let (status, body) = h
            .post_json(
                "/api/bots",
                json!({
                    "name": "bot-a",
                    "corridorId": "cngn-usdt-bsc",
                    "signer": local(TEST_KEY),
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        // Config on disk, with the key owner-only.
        assert!(h.root.join("bot-a/stitch.toml").exists());
        assert!(h.root.join("bot-a/stitch.key").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(h.root.join("bot-a/stitch.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the key must be owner-only");
        }

        // Container created but not started, because `start` defaulted to false.
        assert!(h.docker.exists("stitch-bot-a"));
        let v = Harness::parse(&body);
        assert_eq!(v["bot"]["running"], false);
        assert!(v["message"].as_str().unwrap().contains("dry-run"), "{body}");
        assert!(
            v["message"].as_str().unwrap().contains("Permit2"),
            "create message should name Permit2: {body}"
        );
        assert!(
            v["message"].as_str().unwrap().contains("gas"),
            "create message should mention gas: {body}"
        );
        assert_eq!(
            v["needsPermit2Approval"], true,
            "create must hand off Permit2 regardless of container state: {body}"
        );
        // The response must not echo the key back.
        assert!(!body.contains(TEST_KEY));
    }

    #[tokio::test]
    async fn a_created_bot_gets_the_layout_that_keeps_its_ledger() {
        let h = harness("create-layout");
        h.post_json(
            "/api/bots",
            json!({ "name": "bot-a", "corridorId": "cngn-usdt-bsc", "signer": local(TEST_KEY) }),
        )
        .await;
        let (_, body) = h.get("/api/bots/bot-a").await;
        let v = Harness::parse(&body);
        assert_eq!(v["layout"], "directory");
        assert_eq!(v["origin"], "panel");
        let kinds: Vec<_> = v["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["kind"].as_str().unwrap().to_string())
            .collect();
        assert!(
            !kinds.contains(&"ledgerNotPersisted".to_string()),
            "a panel-created bot must never have the flat layout: {kinds:?}"
        );
    }

    #[tokio::test]
    async fn starting_on_create_is_opt_in() {
        let h = harness("create-start");
        let (status, body) = h
            .post_json(
                "/api/bots",
                json!({
                    "name": "bot-a",
                    "corridorId": "cngn-usdt-bsc",
                    "signer": local(TEST_KEY),
                    "start": true,
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["bot"]["running"], true);
    }

    #[tokio::test]
    async fn start_on_create_goes_through_the_reservation_protocol() {
        // The signer comes from the request body, so it can be the very key an approval
        // is already running against. This path called Docker directly and skipped the
        // exclusion every other launch goes through.
        let h = harness("create-start-reserved");
        // Work out the wallet the new bot will have by creating one first, then reuse
        // the same key for the bot under test.
        h.post_json(
            "/api/bots",
            json!({
                "name": "probe",
                "corridorId": "cngn-usdt-bsc",
                "signer": local(TEST_KEY),
            }),
        )
        .await;
        let wallet = h
            .state
            .bot("probe")
            .await
            .unwrap()
            .wallet()
            .expect("a hot wallet has an address");
        let _approval = h
            .state
            .wallet_locks
            .try_claim(wallet)
            .expect("nothing else holds it");

        let (status, body) = h
            .post_json(
                "/api/bots",
                json!({
                    "name": "bot-a",
                    "corridorId": "cngn-usdt-bsc",
                    "signer": local(TEST_KEY),
                    "start": true,
                }),
            )
            .await;
        // The bot is created — undoing that would be worse than leaving it stopped —
        // but it is not started, and the response says why.
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let v = Harness::parse(&body);
        assert_eq!(v["bot"]["running"], false, "{body}");
        assert!(
            v["message"].as_str().unwrap().contains("not started"),
            "{body}"
        );
        assert!(
            v["message"].as_str().unwrap().contains("wallet is busy"),
            "{body}"
        );
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Start(n) if n == "stitch-bot-a")),
            "{:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn an_existing_directory_is_refused_rather_than_written_into() {
        // The wizard only writes into a directory it creates. That is what makes the
        // ownership handover safe — there is nothing in there but its own files, so it
        // cannot chown an operator's README, backup or recovered ledger to the bot's
        // uid. It is also the atomic claim on the name.
        let h = harness("create-existing-dir");
        let dir = h.root.join("bot-a");
        std::fs::create_dir_all(&dir).unwrap();
        let theirs = dir.join("NOTES.md");
        std::fs::write(&theirs, "mine").unwrap();

        let (status, body) = h
            .post_json(
                "/api/bots",
                json!({
                    "name": "bot-a",
                    "corridorId": "cngn-usdt-bsc",
                    "signer": local(TEST_KEY),
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("already exists"), "{body}");
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "mine");
        // And nothing was written next to it.
        assert!(
            !dir.join("stitch.toml").exists(),
            "no config may be written"
        );
    }

    #[tokio::test]
    async fn a_rejected_signer_does_not_brick_the_name() {
        // The claim is a real directory, so a bad key that fails the writer has to
        // take it back with it — otherwise the operator's typo would make that name
        // permanently unusable.
        let h = harness("create-badkey-retry");
        let bad = json!({
            "name": "bot-a", "corridorId": "cngn-usdt-bsc",
            "signer": local("0xnot-a-key"),
        });
        let (status, body) = h.post_json("/api/bots", bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            !h.root.join("bot-a").exists(),
            "the claimed directory must be released"
        );

        // The same name works once the key is right.
        let (status, body) = h
            .post_json(
                "/api/bots",
                json!({
                    "name": "bot-a",
                    "corridorId": "cngn-usdt-bsc",
                    "signer": local(TEST_KEY),
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    #[tokio::test]
    async fn a_signer_rejected_before_the_writer_also_releases_the_name() {
        // The sibling of the bad-key case above, and the one that regressed when the
        // directory claim moved ahead of validation: `into_setup` rejects "both a key
        // and a phrase" with a plain `?`, which skipped the writer's cleanup entirely
        // and left an empty directory that failed every corrected retry.
        let h = harness("create-bothkeys-retry");
        let (status, body) = h
            .post_json(
                "/api/bots",
                json!({
                    "name": "bot-a",
                    "corridorId": "cngn-usdt-bsc",
                    "signer": {
                        "kind": "local",
                        "privateKey": TEST_KEY,
                        "seedPhrase": "test test test test test test test test test test test junk",
                    },
                }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("not both"), "{body}");
        assert!(
            !h.root.join("bot-a").exists(),
            "the name must be free for the retry"
        );

        // And the corrected request goes through.
        let (status, body) = h
            .post_json(
                "/api/bots",
                json!({
                    "name": "bot-a",
                    "corridorId": "cngn-usdt-bsc",
                    "signer": local(TEST_KEY),
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    #[tokio::test]
    async fn a_duplicate_name_is_refused_before_anything_is_written() {
        let h = harness("create-dupe");
        let payload = json!({
            "name": "bot-a", "corridorId": "cngn-usdt-bsc", "signer": local(TEST_KEY)
        });
        h.post_json("/api/bots", payload.clone()).await;
        let before = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();

        let (status, body) = h.post_json("/api/bots", payload).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        // The existing config is untouched.
        let after = std::fs::read_to_string(h.root.join("bot-a/stitch.toml")).unwrap();
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn a_bad_name_is_refused_with_the_rule_that_broke() {
        let h = harness("create-badname");
        for name in ["../escape", "Bot A", "-lead", "stitch-panel"] {
            let (status, body) = h
                .post_json(
                    "/api/bots",
                    json!({ "name": name, "corridorId": "cngn-usdt-bsc", "signer": local(TEST_KEY) }),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name} must be refused");
            assert!(!body.is_empty());
        }
        // Nothing was created for any of them.
        assert!(!h
            .docker
            .calls()
            .iter()
            .any(|c| matches!(c, crate::panel::docker::fake::Call::Create(_))));
    }

    #[tokio::test]
    async fn an_unknown_corridor_is_refused() {
        let h = harness("create-badcorridor");
        let (status, body) = h
            .post_json(
                "/api/bots",
                json!({ "name": "bot-a", "corridorId": "moon-usdt", "signer": local(TEST_KEY) }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("no corridor"), "{body}");
    }

    #[tokio::test]
    async fn a_bad_private_key_surfaces_the_writers_own_message() {
        let h = harness("create-badkey");
        let (status, body) = h
            .post_json(
                "/api/bots",
                json!({ "name": "bot-a", "corridorId": "cngn-usdt-bsc", "signer": local("0xnope") }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!body.is_empty(), "{body}");
        // Nothing half-written.
        assert!(!h.root.join("bot-a/stitch.toml").exists());
    }

    #[tokio::test]
    async fn a_seed_phrase_is_accepted_and_never_persisted() {
        let h = harness("create-seed");
        let phrase = "test test test test test test test test test test test junk";
        let (status, body) = h
            .post_json(
                "/api/bots",
                json!({
                    "name": "bot-a",
                    "corridorId": "cngn-usdt-bsc",
                    "signer": { "kind": "local", "seedPhrase": phrase },
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        // Only the derived key lands on disk; the phrase itself never does.
        let key = std::fs::read_to_string(h.root.join("bot-a/stitch.key")).unwrap();
        assert!(key.starts_with("0x"));
        assert!(!key.contains("junk"));
        let env = std::fs::read_to_string(h.root.join("bot-a/stitch.env")).unwrap();
        assert!(!env.contains("junk"));
    }

    #[tokio::test]
    async fn sending_both_key_forms_is_refused_rather_than_guessed() {
        let h = harness("create-both");
        let (status, body) = h
            .post_json(
                "/api/bots",
                json!({
                    "name": "bot-a",
                    "corridorId": "cngn-usdt-bsc",
                    "signer": { "kind": "local", "privateKey": TEST_KEY, "seedPhrase": "a b c" },
                }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("not both"), "{body}");
    }

    #[tokio::test]
    async fn a_hot_wallet_with_no_key_at_all_is_refused() {
        let h = harness("create-nokey");
        let (status, body) = h
            .post_json(
                "/api/bots",
                json!({
                    "name": "bot-a",
                    "corridorId": "cngn-usdt-bsc",
                    "signer": { "kind": "local" },
                }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("seed phrase"), "{body}");
    }

    #[tokio::test]
    async fn a_failed_container_create_leaves_a_recoverable_config() {
        let h = harness("create-dockerfail");
        h.docker.fail_next("daemon out of disk");
        let (status, body) = h
            .post_json(
                "/api/bots",
                json!({ "name": "bot-a", "corridorId": "cngn-usdt-bsc", "signer": local(TEST_KEY) }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert!(body.contains("Recreate"), "{body}");
        // The config survived, so the operator can retry without re-entering the key.
        assert!(h.root.join("bot-a/stitch.toml").exists());
        let (_, listed) = h.get("/api/bots").await;
        let v = Harness::parse(&listed);
        assert_eq!(v["bots"][0]["name"], "bot-a");
        assert_eq!(v["bots"][0]["container"], serde_json::Value::Null);
    }
}

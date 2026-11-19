// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Update and roll back a hosted bot, by image digest only.
//!
//! `POST /api/bots/{name}/image` with `{"image": "<repository>@sha256:<digest>"}`.
//! Update and rollback are the same call: the operator names the exact image.
//! Tags (including `latest`) are refused here and again by the broker, because a
//! tag can move after review and a digest can't.
//!
//! Attestation: not verified yet. [`verify_attestation`] is the seam the
//! follow-up PR fills in (check the image's build provenance was signed by
//! Textile's GitHub workflow before the swap). Until then the digest form and
//! repository are all that is checked, and the panel says so in its log and its
//! reply.

use axum::extract::{Path, State};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;

use crate::panel::broker::check_digest_ref;
use crate::panel::docker::swap::swap_outcome;
use crate::panel::http::bots::{claim_for_launch, settle_ambiguous_launch};
use crate::panel::http::{require_actionable, ApiError, AppState};

#[derive(Debug, Deserialize)]
pub struct ImageBody {
    pub image: String,
}

/// Whether `image` may run on this host beyond its digest form.
///
/// TODO(stitch-attestation): verify the GitHub build attestation for `image`
/// (issuer, workflow path, repository, digest) and refuse on any failure. Needs
/// the attestation step in the container workflows first. Returns whether it
/// actually verified anything, so callers can say so.
pub async fn verify_attestation(image: &str) -> Result<bool, String> {
    tracing::warn!(%image, "build attestation not verified: not implemented yet");
    Ok(false)
}

pub async fn replace(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<ImageBody>,
) -> Result<Response, ApiError> {
    let Some(hosted) = state.cfg.profile.hosted() else {
        return Err(ApiError::not_found(
            "this route exists only on hosted panels",
        ));
    };
    let image = body.image.trim().to_string();
    check_digest_ref(&image, &hosted.image_repository).map_err(ApiError::bad_request)?;
    let verified = verify_attestation(&image)
        .await
        .map_err(ApiError::bad_request)?;

    // Held across the swap so a settings save can't write the config the new
    // container is about to load, and so the wallet claimed below is the one the
    // new container will sign with.
    let (_saving, bot) = crate::panel::http::bots::lock_config(&name, &state).await?;
    require_actionable(&bot)?;
    if !bot.layout.persists_ledger() {
        return Err(ApiError::conflict(format!(
            "{name}'s nonce ledger lives inside its container, so recreating it would lose the \
             ledger. Fix the bot's mounts on the host first."
        )));
    }
    if bot.image.as_deref() == Some(image.as_str()) {
        return Err(ApiError::conflict(format!("{name} already runs {image}.")));
    }
    let container = bot
        .require_container()
        .map_err(ApiError::conflict)?
        .to_string();
    // A swap of a bot that's up stops it and starts the replacement, and every
    // live start runs the allowance preflight, which may sign. So it's a launch,
    // like Start, Restart and Recreate: claim the operator wallet for the whole
    // swap, or an approval or another launch on the same wallet reads the same
    // pending nonce and one transaction is lost.
    let wallet = if bot.state.wants_to_be_up() {
        claim_for_launch(&bot, &state).await?
    } else if bot.state.is_terminal() {
        None
    } else {
        // Paused, removing or unknown: its process may still be there, so the
        // swap can't set it aside safely. The broker refuses this too.
        return Err(ApiError::conflict(format!(
            "{name} is {}. Start or stop it before deploying an image.",
            bot.state.as_str()
        )));
    };
    let previous = bot.image.clone().unwrap_or_default();
    if let Err(e) = state.docker.replace_image(&container, &image).await {
        let err = ApiError::internal(&e);
        // A known outcome is definite: the old container never stopped, or it
        // is back and its start returned. Anything else (recovery failed, or the
        // broker's reply was lost) may have left a container starting on the
        // wallet, so settle it before the claim goes, as Start does.
        return Err(match swap_outcome(&e) {
            Some(_) => err,
            None => settle_ambiguous_launch(&state, &container, wallet, err).await,
        });
    }
    drop(wallet);
    tracing::info!(bot = %name, from = %previous, to = %image, attested = verified, "hosted image swap");
    let note = if verified {
        ""
    } else {
        " Build attestation was not checked: that verification isn't implemented yet."
    };
    crate::panel::http::bots::action_response(
        &state,
        &name,
        Some(format!(
            "{name} moved from {previous} to {image}. Watch its logs: the config was not changed \
             with the image.{note}"
        )),
    )
    .await
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Image update detection and panel self-update.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use super::{ApiError, AppState};
use crate::panel::updates::{self, find_self_container, update_target_image};

#[derive(Debug, Deserialize)]
pub struct UpdatesQuery {
    /// When set, skip the process cache and re-query the registry.
    #[serde(default)]
    pub refresh: Option<String>,
}

pub async fn status(
    State(state): State<AppState>,
    Query(q): Query<UpdatesQuery>,
) -> Result<Response, ApiError> {
    let force = q
        .refresh
        .as_deref()
        .is_some_and(|v| v != "0" && v != "false");
    let containers = state.docker.list_all().await.map_err(|e| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("couldn't reach the Docker daemon: {e:#}"),
        )
    })?;
    let fleet = state.fleet().await?;
    let status = updates::check_updates(
        &state.cfg,
        state.docker.as_ref(),
        &fleet,
        &containers,
        force,
    )
    .await;
    Ok(Json(status).into_response())
}

/// Pull a newer panel image and arm a self-recreate. Returns 202; the helper
/// stops this process shortly after.
pub async fn update_panel(State(state): State<AppState>) -> Result<Response, ApiError> {
    let containers = state.docker.list_all().await.map_err(|e| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("couldn't reach the Docker daemon: {e:#}"),
        )
    })?;
    let hostname = std::env::var("HOSTNAME").unwrap_or_default();
    let self_ctr = find_self_container(&containers, &hostname).ok_or_else(|| {
        ApiError::conflict(
            "couldn't find this panel's container (expected name stitch-panel, \
             or HOSTNAME matching a container id) — self-update is unavailable",
        )
    })?;
    let target = update_target_image(&self_ctr.image).ok_or_else(|| {
        ApiError::conflict(
            "this panel image has no registry path, so it can't be pulled for an \
             update — rebuild locally or set PANEL_IMAGE to \
             ghcr.io/textile-protocol/textile-stitch-panel:…",
        )
    })?;

    state
        .docker
        .schedule_image_swap(&self_ctr.name, &target, &state.cfg.docker_socket)
        .await
        .map_err(|e| ApiError::internal(&e.context("scheduling the panel image swap")))?;

    updates::clear_cache();

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "message": format!(
                "Panel update to {target} is armed. This UI will disconnect in a moment \
                 while the container is recreated; reload once it comes back."
            ),
            "targetImage": target,
        })),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::super::testkit::harness;
    use crate::panel::docker::fake::{container, Call};
    use crate::panel::docker::ContainerState;
    use axum::http::StatusCode;

    #[tokio::test]
    async fn updates_status_lists_bots_without_nagging_when_registry_is_unreachable() {
        // Soft-fail: a private host or offline registry must not 500 the UI,
        // and must not offer Update just because a sha-* pin's string differs
        // from the resolved `:latest` target.
        let h = super::super::testkit::harness_with_bot_image(
            "updates-status",
            "ghcr.io/textile-protocol/textile-stitch:sha-deadbeef",
        );
        let mut bot = container("stitch-bot-a", ContainerState::Running);
        bot.labels.insert(
            crate::panel::naming::LABEL_BOT.to_string(),
            "bot-a".to_string(),
        );
        bot.image = "ghcr.io/textile-protocol/textile-stitch:sha-deadbeef".into();
        let corridor = crate::setup::find_corridor("cngn-usdt-bsc").unwrap();
        crate::setup::write_config(
            h.root.join("bot-a"),
            corridor,
            super::super::testkit::TEST_KEY,
        )
        .unwrap();
        bot.mounts = crate::panel::docker::fake::dir_layout_mounts(
            &h.root.join("bot-a").display().to_string(),
        );
        h.docker.add_container(bot);

        crate::panel::updates::clear_cache();
        let (status, body) = h.get("/api/updates?refresh=1").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["bots"].as_array().is_some());
        assert_eq!(
            v["bot"]["targetImage"].as_str(),
            Some("ghcr.io/textile-protocol/textile-stitch:latest"),
            "{body}"
        );
        assert_eq!(
            v["bots"][0]["updateAvailable"], false,
            "registry failure must not report behind via pin≠latest drift: {body}"
        );
        // Pin stays Update-able so the button isn't gated on a live registry check.
        assert_eq!(v["bots"][0]["canUpdate"], true, "{body}");
        assert_eq!(v["bot"]["updateAvailable"], false, "{body}");
        assert!(v["panel"].is_object());
    }

    #[tokio::test]
    async fn panel_update_refuses_a_local_only_image() {
        let h = harness("panel-update-local");
        let mut panel = container("stitch-panel", ContainerState::Running);
        panel.image = "stitch-panel:latest".into();
        h.docker.add_container(panel);

        let (status, body) = h
            .post_json("/api/panel/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("no registry path"), "{body}");
    }

    #[tokio::test]
    async fn panel_update_refuses_when_the_container_cannot_be_found() {
        let h = harness("panel-update-missing");
        let (status, body) = h
            .post_json("/api/panel/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("couldn't find"), "{body}");
    }

    #[tokio::test]
    async fn panel_update_arms_a_swap_for_a_ghcr_image() {
        let h = harness("panel-update-ok");
        let mut panel = container("stitch-panel", ContainerState::Running);
        panel.image = "ghcr.io/textile-protocol/textile-stitch-panel:sha-old".into();
        h.docker.add_container(panel);

        let (status, body) = h
            .post_json("/api/panel/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert!(body.contains("textile-stitch-panel:latest"), "{body}");
        assert!(
            h.docker.calls().iter().any(|c| matches!(
                c,
                Call::ScheduleImageSwap {
                    name,
                    new_image,
                    docker_socket
                } if name == "stitch-panel"
                    && new_image == "ghcr.io/textile-protocol/textile-stitch-panel:latest"
                    && docker_socket == &h.state.cfg.docker_socket.display().to_string()
            )),
            "expected a schedule_image_swap call with the configured socket, got {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn panel_update_helper_binds_the_host_source_of_a_remapped_socket() {
        // STITCH_PANEL_DOCKER_SOCKET is the in-container path. CreateContainer
        // bind sources are host paths — a remap must not mount /docker.sock
        // from the host (it isn't there).
        let h = super::super::testkit::harness_with(
            "panel-update-socket-remap",
            "ghcr.io/textile-protocol/textile-stitch:test",
            Some("/docker.sock"),
        );
        let mut panel = container("stitch-panel", ContainerState::Running);
        panel.image = "ghcr.io/textile-protocol/textile-stitch-panel:sha-old".into();
        panel.mounts = vec![crate::panel::docker::MountInfo {
            source: std::path::PathBuf::from("/var/run/docker.sock"),
            destination: std::path::PathBuf::from("/docker.sock"),
            rw: true,
        }];
        h.docker.add_container(panel);

        let (status, body) = h
            .post_json("/api/panel/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert!(
            h.docker.calls().iter().any(|c| matches!(
                c,
                Call::ScheduleImageSwap { docker_socket, .. }
                    if docker_socket == "/var/run/docker.sock"
            )),
            "helper must bind the host socket source, got {:?}",
            h.docker.calls()
        );
    }

    #[tokio::test]
    async fn panel_update_refuses_when_the_fresh_pull_fails() {
        // ensure_image(refresh) would fall back to a cached local tag; self-update
        // must not — a GHCR outage after /api/updates reported a newer digest
        // would otherwise restart the panel onto the stale copy.
        let h = harness("panel-update-pull-fail");
        let mut panel = container("stitch-panel", ContainerState::Running);
        panel.image = "ghcr.io/textile-protocol/textile-stitch-panel:sha-old".into();
        h.docker.add_container(panel);
        h.docker.fail_image("manifest unknown / rate limited");

        let (status, body) = h
            .post_json("/api/panel/update", serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert!(
            !h.docker
                .calls()
                .iter()
                .any(|c| matches!(c, Call::ScheduleImageSwap { .. })),
            "must not arm a swap after a failed pull: {:?}",
            h.docker.calls()
        );
    }
}

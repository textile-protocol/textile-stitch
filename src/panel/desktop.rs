// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The desktop app's two switches, reachable from the panel's own page.
//!
//! "Keep the Mac awake" and "Start at login" belong to `stitch-desktop`, the
//! menu-bar process that supervises this panel. The panel runs in the system
//! browser, with no bridge into that process, so the two talk through files
//! in the desktop app's own directory:
//!
//! * `desktop-state.json` — written by the app: what the switches are now.
//! * `desktop-request.json` — written by the panel: what the operator asked
//!   for. The app reads it on its status tick, applies it through the same
//!   code the menu uses, deletes it, and rewrites the state file.
//!
//! The panel learns the directory from `STITCH_DESKTOP_DIR`, which only the
//! desktop app sets. Anywhere else (Docker, a server) the endpoint answers
//! `available: false` and the page shows nothing.
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::http::ApiError;

pub const STATE_FILE: &str = "desktop-state.json";
pub const REQUEST_FILE: &str = "desktop-request.json";
pub const DIR_ENV: &str = "STITCH_DESKTOP_DIR";

/// What the desktop app reports. Also the shape the app writes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct DesktopState {
    pub autostart: bool,
    pub keep_awake: bool,
    /// "Keep Mac awake" / "Keep PC awake": the app knows the OS, the panel
    /// does not.
    pub keep_awake_label: String,
}

/// What the operator asked for. Absent fields mean "leave it".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct DesktopRequest {
    pub autostart: Option<bool>,
    pub keep_awake: Option<bool>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopBody {
    /// False outside the desktop app: nothing to show.
    pub available: bool,
    pub autostart: bool,
    pub keep_awake: bool,
    pub keep_awake_label: String,
    /// A request the app has not picked up yet. The page shows the asked-for
    /// value while this is set, so a click does not appear to bounce back.
    pub pending: Option<DesktopRequest>,
}

fn desktop_dir() -> Option<PathBuf> {
    std::env::var_os(DIR_ENV).map(PathBuf::from)
}

pub fn read_state(dir: &Path) -> Option<DesktopState> {
    let raw = std::fs::read_to_string(dir.join(STATE_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn read_request(dir: &Path) -> Option<DesktopRequest> {
    let raw = std::fs::read_to_string(dir.join(REQUEST_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// One writer at a time: two clicks in quick succession run as two handlers,
/// and both reading the prior request before either writes would drop one
/// of the two switches.
static REQUEST_WRITE: Mutex<()> = Mutex::new(());

/// Write the request atomically, merging over one the app has not taken yet.
pub fn write_request(dir: &Path, req: &DesktopRequest) -> anyhow::Result<()> {
    let _serial = REQUEST_WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let prior = read_request(dir).unwrap_or_default();
    let merged = DesktopRequest {
        autostart: req.autostart.or(prior.autostart),
        keep_awake: req.keep_awake.or(prior.keep_awake),
    };
    crate::setup::write_file_atomic(&dir.join(REQUEST_FILE), serde_json::to_vec(&merged)?)
}

fn body(dir: Option<&Path>) -> DesktopBody {
    // No directory: not under the desktop app. A directory with no state file:
    // the app has not reported yet (first seconds after launch, or an older
    // app). Both read as unavailable rather than as switches that do nothing.
    let Some((dir, state)) = dir.and_then(|d| read_state(d).map(|s| (d, s))) else {
        return DesktopBody::default();
    };
    let pending = read_request(dir).filter(|r| r.autostart.is_some() || r.keep_awake.is_some());
    DesktopBody {
        available: true,
        autostart: pending
            .as_ref()
            .and_then(|p| p.autostart)
            .unwrap_or(state.autostart),
        keep_awake: pending
            .as_ref()
            .and_then(|p| p.keep_awake)
            .unwrap_or(state.keep_awake),
        keep_awake_label: state.keep_awake_label,
        pending,
    }
}

/// `GET /api/desktop`: the switches as the app last reported them.
pub async fn get() -> Response {
    Json(body(desktop_dir().as_deref())).into_response()
}

/// `PATCH /api/desktop`: ask the app to flip one or both. Answers with the
/// asked-for values marked pending; the app applies within its tick.
pub async fn patch(Json(req): Json<DesktopRequest>) -> Result<Response, ApiError> {
    let Some(dir) = desktop_dir() else {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "these switches belong to the Stitch desktop app, and this panel is not running under it",
        ));
    };
    if read_state(&dir).is_none() {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "the desktop app has not reported its settings yet; try again in a moment",
        ));
    }
    if req.autostart.is_none() && req.keep_awake.is_none() {
        return Err(ApiError::bad_request("nothing to change"));
    }
    write_request(&dir, &req).map_err(|e| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not hand the request to the desktop app: {e}"),
        )
    })?;
    Ok(Json(body(Some(&dir))).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Self {
            Self(crate::panel::http::testkit::temp_root(&format!(
                "desktop-{tag}"
            )))
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn state(dir: &Path, autostart: bool, keep_awake: bool) {
        std::fs::write(
            dir.join(STATE_FILE),
            serde_json::to_vec(&DesktopState {
                autostart,
                keep_awake,
                keep_awake_label: "Keep Mac awake".into(),
            })
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn no_directory_means_not_available() {
        let b = body(None);
        assert!(!b.available);
    }

    #[test]
    fn a_directory_without_a_state_file_is_not_available_either() {
        let dir = Dir::new("empty");
        assert!(!body(Some(dir.path())).available);
    }

    #[test]
    fn the_state_file_is_what_the_page_sees() {
        let dir = Dir::new("state");
        state(dir.path(), true, false);
        let b = body(Some(dir.path()));
        assert!(b.available && b.autostart && !b.keep_awake);
        assert_eq!(b.keep_awake_label, "Keep Mac awake");
        assert!(b.pending.is_none());
    }

    #[test]
    fn a_request_shows_as_the_asked_for_value_until_the_app_takes_it() {
        let dir = Dir::new("request");
        state(dir.path(), false, false);
        write_request(
            dir.path(),
            &DesktopRequest {
                keep_awake: Some(true),
                autostart: None,
            },
        )
        .unwrap();
        let b = body(Some(dir.path()));
        assert!(b.keep_awake, "the page shows what was asked for");
        assert!(!b.autostart, "untouched switch keeps the app's value");
        assert!(b.pending.is_some());

        // A second request merges rather than replaces.
        write_request(
            dir.path(),
            &DesktopRequest {
                autostart: Some(true),
                keep_awake: None,
            },
        )
        .unwrap();
        let r = read_request(dir.path()).unwrap();
        assert_eq!(r.keep_awake, Some(true));
        assert_eq!(r.autostart, Some(true));
    }
}

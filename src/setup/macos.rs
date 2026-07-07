// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Detect where the macOS app is running from and, on request, move it into
//! /Applications.
//!
//! A freshly-downloaded, quarantined app launched in place — straight from the
//! mounted DMG, or from ~/Downloads after unzipping — is subject to Gatekeeper
//! App Translocation: macOS runs it from a randomized, read-only mount. That
//! breaks two things stitch-setup relies on: finding the sibling `stitch` binary
//! by relative path (see [`crate::setup::find_stitch_binary`]) and the in-app
//! Update button. Dragging the app into /Applications is a Finder "move", which
//! disables translocation and gives the app a stable, writable home. The DMG
//! nudges most operators into doing that; this is the backstop for the ones who
//! double-click it in place instead.

use std::path::{Path, PathBuf};

/// Where the running `.app` bundle lives, install-wise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacInstall {
    /// Path to the running `.app` bundle.
    pub bundle: PathBuf,
    /// True when the bundle sits on an App Translocation mount.
    pub translocated: bool,
    /// True when the bundle already lives in the system or user Applications folder.
    pub in_applications: bool,
}

impl MacInstall {
    /// Whether to nudge the operator to move the app: it's translocated, or it
    /// lives outside an Applications folder (Downloads, Desktop, a mounted DMG…).
    pub fn needs_move(&self) -> bool {
        self.translocated || !self.in_applications
    }

    /// Where the app would be installed: /Applications/<bundle name>.
    pub fn target(&self) -> PathBuf {
        Path::new("/Applications").join(bundle_name(&self.bundle))
    }

    /// Copy the bundle into /Applications (replacing any existing copy) and strip
    /// the quarantine flag so the installed copy launches without re-translocating.
    /// Returns the installed path. It's a copy, not a rename: a translocated or
    /// DMG source is read-only / throwaway and may be on another volume.
    pub fn install(&self) -> anyhow::Result<PathBuf> {
        use anyhow::{bail, Context};
        let target = self.target();
        if target.exists() {
            std::fs::remove_dir_all(&target)
                .with_context(|| format!("removing the old {}", target.display()))?;
        }
        // ditto preserves the bundle's symlinks, permissions, extended attributes
        // and code signature — a hand-rolled recursive copy wouldn't.
        let status = std::process::Command::new("/usr/bin/ditto")
            .arg(&self.bundle)
            .arg(&target)
            .status()
            .context("running ditto to copy the app")?;
        if !status.success() {
            bail!(
                "couldn't copy Stitch into Applications — do you have permission to write there?"
            );
        }
        // Best-effort: clear quarantine so Gatekeeper doesn't re-translocate the
        // installed copy on its first launch.
        let _ = std::process::Command::new("/usr/bin/xattr")
            .args(["-dr", "com.apple.quarantine"])
            .arg(&target)
            .status();
        Ok(target)
    }
}

/// Launch the app at `path` (so the freshly-installed copy comes up as this one
/// exits).
pub fn open(path: &Path) {
    let _ = std::process::Command::new("/usr/bin/open")
        .arg(path)
        .spawn();
}

/// Detect the running app's install location. `None` when we're not inside a
/// `.app` bundle — a dev build, the bare `stitch-setup` binary, or any non-macOS
/// platform — because then there's nothing to nudge about.
pub fn detect() -> Option<MacInstall> {
    // Only macOS has App Translocation and an Applications folder to install into;
    // the `if cfg!` (not `#[cfg]`) keeps the pure helpers referenced — and their
    // tests running — on every platform.
    if cfg!(target_os = "macos") {
        let exe = std::env::current_exe().ok()?;
        let bundle = find_app_bundle(&exe)?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Some(MacInstall {
            translocated: is_translocated(&bundle),
            in_applications: is_in_applications(&bundle, home.as_deref()),
            bundle,
        })
    } else {
        None
    }
}

/// The nearest ancestor of `exe` whose name ends in `.app` (the bundle root), or
/// `None` when the executable isn't inside one.
fn find_app_bundle(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".app"))
        })
        .map(Path::to_path_buf)
}

/// The bundle's own directory name, defaulting to `Stitch.app` for a pathological
/// bundle path with no final component.
fn bundle_name(bundle: &Path) -> PathBuf {
    bundle
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("Stitch.app"))
}

/// Gatekeeper mounts translocated apps under a path with an `AppTranslocation`
/// component (e.g. /private/var/folders/…/AppTranslocation/<uuid>/d/Stitch.app).
fn is_translocated(bundle: &Path) -> bool {
    bundle
        .components()
        .any(|c| c.as_os_str() == "AppTranslocation")
}

/// True when the bundle sits under the system `/Applications` or the user's
/// `~/Applications` (nested subfolders of either count too).
fn is_in_applications(bundle: &Path, home: Option<&Path>) -> bool {
    bundle.starts_with("/Applications")
        || home.is_some_and(|h| bundle.starts_with(h.join("Applications")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_bundle_root_above_the_executable() {
        let exe = Path::new("/Applications/Stitch.app/Contents/MacOS/stitch-setup");
        assert_eq!(
            find_app_bundle(exe),
            Some(PathBuf::from("/Applications/Stitch.app"))
        );
    }

    #[test]
    fn no_bundle_for_a_bare_binary() {
        // A dev build or the standalone stitch-setup binary isn't in a .app.
        assert_eq!(
            find_app_bundle(Path::new("/tmp/target/debug/stitch-setup")),
            None
        );
    }

    #[test]
    fn detects_translocation_by_path() {
        let translocated =
            Path::new("/private/var/folders/aa/bb/T/AppTranslocation/1234-UUID/d/Stitch.app");
        assert!(is_translocated(translocated));
        assert!(!is_translocated(Path::new("/Applications/Stitch.app")));
        // A folder literally named "AppTranslocationNotes" must not trip it.
        assert!(!is_translocated(Path::new(
            "/Users/x/AppTranslocationNotes/Stitch.app"
        )));
    }

    #[test]
    fn recognizes_system_and_user_applications() {
        let home = Path::new("/Users/x");
        assert!(is_in_applications(
            Path::new("/Applications/Stitch.app"),
            Some(home)
        ));
        assert!(is_in_applications(
            Path::new("/Applications/Utilities/Stitch.app"),
            Some(home)
        ));
        assert!(is_in_applications(
            Path::new("/Users/x/Applications/Stitch.app"),
            Some(home)
        ));
    }

    #[test]
    fn downloads_and_dmg_are_not_installed() {
        let home = Path::new("/Users/x");
        assert!(!is_in_applications(
            Path::new("/Users/x/Downloads/Stitch.app"),
            Some(home)
        ));
        assert!(!is_in_applications(
            Path::new("/Volumes/Stitch/Stitch.app"),
            Some(home)
        ));
        // A path that only shares the /Applications prefix by string, not by
        // component, must not count.
        assert!(!is_in_applications(
            Path::new("/ApplicationsOld/Stitch.app"),
            Some(home)
        ));
    }

    #[test]
    fn needs_move_covers_translocated_and_elsewhere() {
        let base = PathBuf::from("/Users/x/Downloads/Stitch.app");
        assert!(MacInstall {
            bundle: base.clone(),
            translocated: false,
            in_applications: false,
        }
        .needs_move());
        assert!(MacInstall {
            bundle: base.clone(),
            translocated: true,
            in_applications: false,
        }
        .needs_move());
        // Already in /Applications and not translocated → leave it alone.
        assert!(!MacInstall {
            bundle: PathBuf::from("/Applications/Stitch.app"),
            translocated: false,
            in_applications: true,
        }
        .needs_move());
    }

    #[test]
    fn target_is_in_system_applications() {
        let install = MacInstall {
            bundle: PathBuf::from("/Volumes/Stitch/Stitch.app"),
            translocated: false,
            in_applications: false,
        };
        assert_eq!(install.target(), PathBuf::from("/Applications/Stitch.app"));
    }
}

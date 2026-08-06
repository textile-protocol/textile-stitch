// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Hand a verified desktop update to the platform after the main process exits.

use std::path::Path;

use anyhow::{Context, Result};

/// Mount the macOS image, or launch a detached helper that replaces the
/// Windows/Linux release binaries after this process exits and then relaunches
/// Stitch.
pub fn stage(archive: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("/usr/bin/open")
            .arg(archive)
            .spawn()
            .context("mounting the Stitch disk image")?;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        stage_windows(archive)
    }

    #[cfg(target_os = "linux")]
    {
        stage_linux(archive)
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = archive;
        anyhow::bail!("desktop updates are not supported on this platform")
    }
}

#[cfg(target_os = "windows")]
fn stage_windows(archive: &Path) -> Result<()> {
    let install_dir = std::env::current_exe()
        .context("finding the Stitch executable")?
        .parent()
        .context("Stitch executable has no parent directory")?
        .to_path_buf();
    let script = archive.with_file_name("install-stitch-update.ps1");
    std::fs::write(&script, windows_script())
        .with_context(|| format!("writing {}", script.display()))?;

    let mut command = std::process::Command::new("powershell");
    command.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-WindowStyle",
        "Hidden",
        "-File",
    ]);
    command
        .arg(&script)
        .arg(std::process::id().to_string())
        .arg(archive)
        .arg(install_dir);
    crate::win_cmd::no_window(&mut command);
    command.spawn().context("starting the Windows updater")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn stage_linux(archive: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let install_dir = std::env::current_exe()
        .context("finding the Stitch executable")?
        .parent()
        .context("Stitch executable has no parent directory")?
        .to_path_buf();
    let script = archive.with_file_name("install-stitch-update.sh");
    std::fs::write(&script, linux_script())
        .with_context(|| format!("writing {}", script.display()))?;
    let mut permissions = std::fs::metadata(&script)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&script, permissions)?;

    std::process::Command::new(&script)
        .arg(std::process::id().to_string())
        .arg(archive)
        .arg(install_dir)
        .spawn()
        .context("starting the Linux updater")?;
    Ok(())
}

#[cfg(any(test, target_os = "windows"))]
fn windows_script() -> &'static str {
    r#"$ErrorActionPreference = "Stop"
$stitchPid = [int]$args[0]
$archive = $args[1]
$installDir = $args[2]
Wait-Process -Id $stitchPid -ErrorAction SilentlyContinue
$unpack = Join-Path ([System.IO.Path]::GetTempPath()) ("stitch-update-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $unpack | Out-Null
try {
  Expand-Archive -LiteralPath $archive -DestinationPath $unpack -Force
  $desktop = Get-ChildItem -Path $unpack -Filter "stitch-desktop.exe" -Recurse | Select-Object -First 1
  if (-not $desktop) { throw "The update archive does not contain stitch-desktop.exe" }
  Get-ChildItem -Path $desktop.DirectoryName -Filter "stitch*.exe" |
    Copy-Item -Destination $installDir -Force
  Start-Process -FilePath (Join-Path $installDir "stitch-desktop.exe")
} finally {
  Remove-Item -LiteralPath $unpack -Recurse -Force -ErrorAction SilentlyContinue
}
"#
}

#[cfg(any(test, target_os = "linux"))]
fn linux_script() -> &'static str {
    r#"#!/bin/sh
set -eu
stitch_pid="$1"
archive="$2"
install_dir="$3"
while kill -0 "$stitch_pid" 2>/dev/null; do sleep 1; done
unpack="$(mktemp -d "${TMPDIR:-/tmp}/stitch-update.XXXXXX")"
trap 'rm -rf "$unpack"' EXIT
tar -xJf "$archive" -C "$unpack"
desktop="$(find "$unpack" -type f -name stitch-desktop -print -quit)"
[ -n "$desktop" ] || { echo "The update archive does not contain stitch-desktop" >&2; exit 1; }
source_dir="$(dirname "$desktop")"
for binary in stitch stitch-panel stitch-desktop; do
  install -m 755 "$source_dir/$binary" "$install_dir/$binary"
done
nohup "$install_dir/stitch-desktop" >/dev/null 2>&1 &
"#
}

#[cfg(test)]
mod tests {
    use super::{linux_script, windows_script};

    #[test]
    fn windows_helper_waits_replaces_and_relaunches() {
        let script = windows_script();
        assert!(script.contains("Wait-Process"));
        assert!(script.contains("Copy-Item"));
        assert!(script.contains("Start-Process"));
    }

    #[test]
    fn linux_helper_waits_replaces_and_relaunches() {
        let script = linux_script();
        assert!(script.contains("kill -0"));
        assert!(script.contains("install -m 755"));
        assert!(script.contains("nohup"));
    }
}

//! Register / unregister OS "start at login" for the desktop tray app.
//!
//! Launch args always include `--autostart` so a login start does not show the
//! control window. The panel still comes up and restores bots that were
//! `wanted_up` when the previous session stopped (see process runtime
//! persistence).

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(all(unix, not(target_os = "macos")))]
const APP_ID: &str = "io.textile.stitch-desktop";

#[cfg(target_os = "macos")]
const LAUNCH_AGENT_LABEL: &str = "io.textile.stitch-desktop";

#[cfg(target_os = "windows")]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(target_os = "windows")]
const VALUE_NAME: &str = "TextileStitchDesktop";

pub fn is_enabled() -> bool {
    match platform_is_enabled() {
        Ok(v) => v,
        Err(err) => {
            eprintln!("autostart status check failed: {err}");
            false
        }
    }
}

pub fn set_enabled(enabled: bool) -> io::Result<()> {
    if enabled {
        enable()
    } else {
        disable()
    }
}

fn current_exe() -> io::Result<PathBuf> {
    env::current_exe()
}

#[cfg(target_os = "macos")]
fn launch_agent_plist_path() -> io::Result<PathBuf> {
    let home = env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn launch_agent_plist(exe: &Path) -> String {
    let exe_xml = xml_escape(&exe.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCH_AGENT_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe_xml}</string>
    <string>--autostart</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <false/>
  <key>ProcessType</key>
  <string>Interactive</string>
</dict>
</plist>
"#
    )
}

#[cfg(target_os = "macos")]
fn platform_is_enabled() -> io::Result<bool> {
    let path = launch_agent_plist_path()?;
    if !path.is_file() {
        return Ok(false);
    }
    let contents = fs::read_to_string(&path)?;
    Ok(contents.contains(LAUNCH_AGENT_LABEL) && contents.contains("--autostart"))
}

#[cfg(target_os = "macos")]
fn enable() -> io::Result<()> {
    let exe = current_exe()?;
    let path = launch_agent_plist_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, launch_agent_plist(&exe))?;
    // Best-effort: load immediately so login-item state matches without reboot.
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &path.display().to_string()])
        .status();
    let status = std::process::Command::new("launchctl")
        .args(["load", "-w", &path.display().to_string()])
        .status()?;
    if !status.success() {
        // Plist is written; load can fail in sandboxed/CI environments.
        eprintln!("warning: launchctl load returned {status}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn disable() -> io::Result<()> {
    let path = launch_agent_plist_path()?;
    if path.is_file() {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", "-w", &path.display().to_string()])
            .status();
        fs::remove_file(&path)?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn platform_is_enabled() -> io::Result<bool> {
    use std::process::Command;
    let mut cmd = Command::new("reg");
    cmd.args(["query", RUN_KEY, "/v", VALUE_NAME]);
    crate::win_cmd::no_window(&mut cmd);
    let output = cmd.output()?;
    if !output.status.success() {
        return Ok(false);
    }
    Ok(String::from_utf8_lossy(&output.stdout).contains(VALUE_NAME))
}

#[cfg(target_os = "windows")]
fn enable() -> io::Result<()> {
    use std::process::Command;
    let exe = current_exe()?;
    let value = format!("\"{}\" --autostart", exe.display());
    let mut cmd = Command::new("reg");
    cmd.args([
        "add", RUN_KEY, "/v", VALUE_NAME, "/t", "REG_SZ", "/d", &value, "/f",
    ]);
    crate::win_cmd::no_window(&mut cmd);
    let status = cmd.status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "reg add failed with status {status}"
        )));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn disable() -> io::Result<()> {
    use std::process::Command;
    let mut cmd = Command::new("reg");
    cmd.args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"]);
    crate::win_cmd::no_window(&mut cmd);
    let status = cmd.status()?;
    // Missing value is fine (already disabled).
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        if code != 1 {
            return Err(io::Error::other(format!(
                "reg delete failed with status {status}"
            )));
        }
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn xdg_autostart_path() -> io::Result<PathBuf> {
    let config_home = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME / XDG_CONFIG_HOME unset"))?;
    Ok(config_home
        .join("autostart")
        .join(format!("{APP_ID}.desktop")))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn xdg_autostart_desktop(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name=Textile Stitch\n\
         Comment=Local Stitch panel (tray + window)\n\
         Exec=\"{}\" --autostart\n\
         Terminal=false\n\
         Categories=Utility;Network;\n\
         X-GNOME-Autostart-enabled=true\n\
         X-Textile-Stitch-Autostart=1\n",
        exe.display()
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_is_enabled() -> io::Result<bool> {
    let path = xdg_autostart_path()?;
    if !path.is_file() {
        return Ok(false);
    }
    let contents = fs::read_to_string(&path)?;
    Ok(contents.contains("X-Textile-Stitch-Autostart=1") && contents.contains("--autostart"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn enable() -> io::Result<()> {
    let exe = current_exe()?;
    let path = xdg_autostart_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, xdg_autostart_desktop(&exe))?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn disable() -> io::Result<()> {
    let path = xdg_autostart_path()?;
    if path.is_file() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_contains_autostart_flag() {
        let plist = launch_agent_plist(Path::new(
            "/Applications/Textile Stitch.app/Contents/MacOS/stitch-desktop",
        ));
        assert!(plist.contains("--autostart"));
        assert!(plist.contains(LAUNCH_AGENT_LABEL));
        assert!(plist.contains("&apos;") || !plist.contains('\''));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn desktop_file_contains_autostart_flag() {
        let desktop = xdg_autostart_desktop(Path::new("/usr/local/bin/stitch-desktop"));
        assert!(desktop.contains("--autostart"));
        assert!(desktop.contains("X-Textile-Stitch-Autostart=1"));
        assert!(desktop.contains(APP_ID) || desktop.contains("Textile Stitch"));
    }
}

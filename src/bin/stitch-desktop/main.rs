// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Stitch desktop — menu bar / system tray controller for the local panel.
//!
//! Starts `stitch-panel` in process runtime (no Docker), opens the browser, and
//! offers start/stop/update without a terminal. The browser UI is the same Stitch
//! panel used on servers.
//!
//! Pass `--autostart` (set by the OS login item) to skip opening a browser tab;
//! the panel still starts and restores bots that were left running.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;
mod migrate;
mod password;
mod paths;
mod supervise;

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

use crate::supervise::PanelSupervisor;

const PANEL_URL: &str = "http://127.0.0.1:8420";

#[derive(Debug)]
enum UserEvent {
    Menu(tray_icon::menu::MenuId),
}

fn main() {
    if let Err(e) = run() {
        eprintln!("stitch-desktop: {e:#}");
        std::process::exit(1);
    }
}

fn launched_via_autostart() -> bool {
    std::env::args().any(|a| a == "--autostart")
}

fn run() -> Result<()> {
    let quiet_launch = launched_via_autostart();

    // Gatekeeper App Translocation breaks sibling `stitch-panel` / `stitch`
    // lookup. If we're still on the DMG or in Downloads, copy into
    // /Applications and relaunch from there.
    if let Some(install) = stitch_bot::setup::macos::detect() {
        if install.needs_move() {
            match install.install() {
                Ok(target) => {
                    eprintln!("Moved Stitch to {} — relaunching.", target.display());
                    stitch_bot::setup::macos::open(&target);
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("couldn't install into Applications: {e:#}");
                }
            }
        }
    }

    let paths = paths::DesktopPaths::resolve()?;
    paths.ensure_dirs()?;
    // Upgrade path from stitch-setup: import the old single-bot config into
    // `<data_root>/bots/<id>` before the panel opens an empty fleet.
    if let Err(e) = migrate::import_legacy_desktop_config(&paths) {
        eprintln!("stitch-desktop: legacy config import skipped: {e:#}");
    }
    let password = password::ensure_panel_password(&paths)?;

    let supervisor = Arc::new(Mutex::new(PanelSupervisor::new(paths.clone())?));
    {
        let mut s = supervisor.lock().unwrap();
        s.start().context("starting the local Stitch panel")?;
    }
    // Interactive launches open the browser; login autostart stays in the tray.
    // Bots that were `wanted_up` when the panel last stopped come back via the
    // process runtime's persisted state — no extra work here.
    if !quiet_launch {
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(800));
            let _ = open_url(PANEL_URL);
        });
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = proxy.send_event(UserEvent::Menu(event.id));
    }));

    let open_item = MenuItem::new("Open Stitch", true, None);
    let start_item = MenuItem::new("Start panel", true, None);
    let stop_item = MenuItem::new("Stop panel", true, None);
    let autostart_item = CheckMenuItem::new("Start at login", true, autostart::is_enabled(), None);
    let copy_pw_item = MenuItem::new("Copy panel password", true, None);
    let update_item = MenuItem::new("Check for updates…", true, None);
    let quit_item = MenuItem::new("Quit Stitch", true, None);

    let open_id = open_item.id().clone();
    let start_id = start_item.id().clone();
    let stop_id = stop_item.id().clone();
    let autostart_id = autostart_item.id().clone();
    let copy_pw_id = copy_pw_item.id().clone();
    let update_id = update_item.id().clone();
    let quit_id = quit_item.id().clone();

    let menu = Menu::new();
    menu.append(&open_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&start_item)?;
    menu.append(&stop_item)?;
    menu.append(&autostart_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&copy_pw_item)?;
    menu.append(&update_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit_item)?;

    let icon = tray_icon_from_embedded().unwrap_or_else(fallback_icon);
    let mut _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Stitch")
        .with_icon(icon)
        .build()
        .context("creating the menu bar / tray icon")?;

    let password = Arc::new(password);

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {}
            Event::UserEvent(UserEvent::Menu(id)) => {
                if id == open_id {
                    let _ = open_url(PANEL_URL);
                } else if id == start_id {
                    if let Ok(mut s) = supervisor.lock() {
                        if let Err(e) = s.start() {
                            eprintln!("start failed: {e:#}");
                        }
                    }
                } else if id == stop_id {
                    if let Ok(mut s) = supervisor.lock() {
                        if let Err(e) = s.stop() {
                            eprintln!("stop failed: {e:#}");
                        }
                    }
                } else if id == autostart_id {
                    // muda toggles the checkmark before we see the event.
                    let enabled = autostart_item.is_checked();
                    if let Err(e) = autostart::set_enabled(enabled) {
                        eprintln!("start at login failed: {e}");
                        autostart_item.set_checked(!enabled);
                    }
                } else if id == copy_pw_id {
                    if let Err(e) = copy_to_clipboard(&password) {
                        eprintln!("copy password failed: {e:#}");
                    }
                } else if id == update_id {
                    let _ = open_url(stitch_bot::update::RELEASES_PAGE);
                } else if id == quit_id {
                    if let Ok(mut s) = supervisor.lock() {
                        let _ = s.stop();
                    }
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::LoopDestroyed => {
                if let Ok(mut s) = supervisor.lock() {
                    let _ = s.stop();
                }
            }
            _ => {}
        }
    });
}

fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .status()
            .context("open")?;
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .status()
            .context("start")?;
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .status()
            .context("xdg-open")?;
        return Ok(());
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    {
        let _ = url;
        anyhow::bail!("cannot open URLs on this platform")
    }
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write;

    fn write_and_close(child: &mut std::process::Child, text: &str) -> Result<()> {
        // Clipboard helpers read stdin to EOF before exiting — leave the pipe
        // open across wait() and they hang forever.
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
            stdin.flush()?;
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        let mut child = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("pbcopy")?;
        write_and_close(&mut child, text)?;
        let status = child.wait()?;
        anyhow::ensure!(status.success(), "pbcopy failed");
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        let mut child = std::process::Command::new("clip")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("clip")?;
        write_and_close(&mut child, text)?;
        let status = child.wait()?;
        anyhow::ensure!(status.success(), "clip failed");
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Ok(mut child) = std::process::Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            write_and_close(&mut child, text)?;
            let _ = child.wait();
            return Ok(());
        }
        if let Ok(mut child) = std::process::Command::new("wl-copy")
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            write_and_close(&mut child, text)?;
            let _ = child.wait();
            return Ok(());
        }
        anyhow::bail!("no clipboard tool (xclip/wl-copy); password is in the panel.password file")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    {
        let _ = text;
        anyhow::bail!("clipboard unsupported on this platform")
    }
}

fn tray_icon_from_embedded() -> Option<Icon> {
    // 32×32 RGBA teal square — light, no asset pipeline.
    let size = 32u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for px in rgba.chunks_exact_mut(4) {
        px[0] = 0x14;
        px[1] = 0xb8;
        px[2] = 0xa6;
        px[3] = 0xff;
    }
    Icon::from_rgba(rgba, size, size).ok()
}

fn fallback_icon() -> Icon {
    tray_icon_from_embedded().expect("fallback icon")
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Stitch desktop — menu bar / system tray controller with a Dock icon and
//! control window (macOS Dock can be hidden via a preference).
//!
//! Starts `stitch-panel` in process runtime (no Docker), opens the browser, and
//! offers start/stop/update without a terminal. The browser UI is the same Stitch
//! panel used on servers; the desktop window mirrors tray actions.
//!
//! Pass `--autostart` (set by the OS login item) to skip opening a browser tab
//! and the control window; the panel still starts and restores bots that were
//! left running.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;
mod control_ui;
mod migrate;
mod password;
mod paths;
mod prefs;
mod supervise;

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::{Window, WindowBuilder};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use wry::WebViewBuilder;

use crate::prefs::DesktopPrefs;
use crate::supervise::PanelSupervisor;

const PANEL_URL: &str = "http://127.0.0.1:8420";
const WINDOW_TITLE: &str = "Stitch";
const WINDOW_INNER_WIDTH: f64 = 380.0;
const WINDOW_INNER_HEIGHT: f64 = 520.0;

#[derive(Debug)]
enum UserEvent {
    Menu(tray_icon::menu::MenuId),
    Ipc(String),
    /// Periodic poll so the control window reflects panel exits without a click.
    RefreshStatus,
}

const STATUS_POLL_SECS: u64 = 2;

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
    let mut prefs = DesktopPrefs::load(&paths);

    let supervisor = Arc::new(Mutex::new(PanelSupervisor::new(paths.clone())?));

    #[cfg(target_os = "linux")]
    {
        gtk::init().context("initializing GTK for the Stitch window")?;
    }

    // `mut` is required on macOS for set_activation_policy before run.
    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        // Dock icon on by default; Accessory only when the user hides it.
        let policy = if prefs.hide_dock_icon {
            ActivationPolicy::Accessory
        } else {
            ActivationPolicy::Regular
        };
        event_loop.set_activation_policy(policy);
    }

    let proxy = event_loop.create_proxy();
    let ipc_proxy = proxy.clone();
    let status_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = proxy.send_event(UserEvent::Menu(event.id));
    }));
    // The event loop is Wait-based — without a timer, a crashed panel leaves the
    // control window stuck on "Panel running" until the next user action.
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(STATUS_POLL_SECS));
        if status_proxy.send_event(UserEvent::RefreshStatus).is_err() {
            break;
        }
    });

    let show_item = MenuItem::new("Show Stitch window", true, None);
    let open_item = MenuItem::new("Open Stitch", true, None);
    let start_item = MenuItem::new("Start panel", true, None);
    let stop_item = MenuItem::new("Stop panel", true, None);
    let autostart_item = CheckMenuItem::new("Start at login", true, autostart::is_enabled(), None);
    #[cfg(target_os = "macos")]
    let hide_dock_item = CheckMenuItem::new("Hide Dock icon", true, prefs.hide_dock_icon, None);
    let copy_pw_item = MenuItem::new("Copy panel password", true, None);
    let update_item = MenuItem::new("Check for updates…", true, None);
    let quit_item = MenuItem::new("Quit Stitch", true, None);

    let show_id = show_item.id().clone();
    let open_id = open_item.id().clone();
    let start_id = start_item.id().clone();
    let stop_id = stop_item.id().clone();
    let autostart_id = autostart_item.id().clone();
    #[cfg(target_os = "macos")]
    let hide_dock_id = hide_dock_item.id().clone();
    let copy_pw_id = copy_pw_item.id().clone();
    let update_id = update_item.id().clone();
    let quit_id = quit_item.id().clone();

    let menu = Menu::new();
    menu.append(&show_item)?;
    menu.append(&open_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&start_item)?;
    menu.append(&stop_item)?;
    menu.append(&autostart_item)?;
    #[cfg(target_os = "macos")]
    menu.append(&hide_dock_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&copy_pw_item)?;
    menu.append(&update_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit_item)?;

    let panel_running = supervisor
        .lock()
        .map(|mut s| s.is_running())
        .unwrap_or(false);
    let window = WindowBuilder::new()
        .with_title(WINDOW_TITLE)
        .with_inner_size(tao::dpi::LogicalSize::new(
            WINDOW_INNER_WIDTH,
            WINDOW_INNER_HEIGHT,
        ))
        .with_visible(!quiet_launch)
        .build(&event_loop)
        .context("creating Stitch window")?;
    let window_id = window.id();

    #[cfg(target_os = "macos")]
    let hide_dock_row = true;
    #[cfg(not(target_os = "macos"))]
    let hide_dock_row = false;

    let html = control_ui::html(
        autostart_item.is_checked(),
        prefs.hide_dock_icon,
        panel_running,
        hide_dock_row,
    );
    let webview = WebViewBuilder::new()
        .with_html(&html)
        .with_ipc_handler(move |req| {
            let _ = ipc_proxy.send_event(UserEvent::Ipc(req.body().to_string()));
        })
        .build(&window)
        .context("creating Stitch control webview")?;

    let password = Arc::new(password);
    // tray-icon requires the macOS event loop to be running before TrayIcon::new.
    let mut tray: Option<TrayIcon> = None;
    let mut started_panel = false;
    let mut window: Option<Window> = Some(window);
    let mut webview = Some(webview);

    event_loop.run(move |event, elwt, control_flow| {
        *control_flow = ControlFlow::Wait;

        #[cfg(target_os = "linux")]
        {
            while gtk::events_pending() {
                gtk::main_iteration_do(false);
            }
        }

        match event {
            Event::NewEvents(StartCause::Init) => {
                if tray.is_none() {
                    let icon = tray_icon_from_embedded().unwrap_or_else(fallback_icon);
                    match TrayIconBuilder::new()
                        .with_menu(Box::new(menu.clone()))
                        .with_tooltip("Stitch")
                        .with_icon(icon)
                        .build()
                    {
                        Ok(t) => tray = Some(t),
                        Err(e) => {
                            eprintln!("stitch-desktop: creating menu bar icon failed: {e:#}");
                            *control_flow = ControlFlow::Exit;
                            return;
                        }
                    }
                }
                if !started_panel {
                    started_panel = true;
                    let start_result = {
                        let mut s = supervisor.lock().unwrap();
                        s.start().context("starting the local Stitch panel")
                    };
                    match start_result {
                        Ok(()) => {
                            // Interactive launches open the browser; login
                            // autostart stays in the tray / Dock.
                            if !quiet_launch {
                                std::thread::spawn(|| {
                                    std::thread::sleep(std::time::Duration::from_millis(800));
                                    let _ = open_url(PANEL_URL);
                                });
                            }
                        }
                        Err(e) => {
                            // Keep the tray alive so the user can Quit / retry Start.
                            eprintln!("stitch-desktop: {e:#}");
                        }
                    }
                    sync_control_ui(
                        webview.as_ref(),
                        &autostart_item,
                        prefs.hide_dock_icon,
                        &supervisor,
                    );
                }
            }
            Event::UserEvent(UserEvent::Menu(id)) => {
                if id == show_id {
                    show_control_window(window.as_ref());
                } else if id == open_id {
                    let _ = open_url(PANEL_URL);
                } else if id == start_id {
                    if let Ok(mut s) = supervisor.lock() {
                        if let Err(e) = s.start() {
                            eprintln!("start failed: {e:#}");
                        }
                    }
                    sync_control_ui(
                        webview.as_ref(),
                        &autostart_item,
                        prefs.hide_dock_icon,
                        &supervisor,
                    );
                } else if id == stop_id {
                    if let Ok(mut s) = supervisor.lock() {
                        if let Err(e) = s.stop() {
                            eprintln!("stop failed: {e:#}");
                        }
                    }
                    sync_control_ui(
                        webview.as_ref(),
                        &autostart_item,
                        prefs.hide_dock_icon,
                        &supervisor,
                    );
                } else if id == autostart_id {
                    // muda toggles the checkmark before we see the event.
                    let enabled = autostart_item.is_checked();
                    if let Err(e) = autostart::set_enabled(enabled) {
                        eprintln!("start at login failed: {e}");
                        autostart_item.set_checked(!enabled);
                    }
                    sync_control_ui(
                        webview.as_ref(),
                        &autostart_item,
                        prefs.hide_dock_icon,
                        &supervisor,
                    );
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
                #[cfg(target_os = "macos")]
                if id == hide_dock_id {
                    let hide = hide_dock_item.is_checked();
                    prefs.hide_dock_icon = hide;
                    if let Err(e) = prefs.save(&paths) {
                        eprintln!("saving desktop prefs failed: {e:#}");
                    }
                    apply_dock_policy(elwt, hide);
                    sync_control_ui(
                        webview.as_ref(),
                        &autostart_item,
                        prefs.hide_dock_icon,
                        &supervisor,
                    );
                }
            }
            Event::UserEvent(UserEvent::Ipc(msg)) => {
                handle_ipc(
                    &msg,
                    &supervisor,
                    &password,
                    &autostart_item,
                    #[cfg(target_os = "macos")]
                    &hide_dock_item,
                    &mut prefs,
                    &paths,
                    elwt,
                    control_flow,
                    webview.as_ref(),
                );
            }
            Event::UserEvent(UserEvent::RefreshStatus) => {
                sync_control_ui(
                    webview.as_ref(),
                    &autostart_item,
                    prefs.hide_dock_icon,
                    &supervisor,
                );
            }
            Event::WindowEvent {
                window_id: id,
                event: WindowEvent::CloseRequested,
                ..
            } if id == window_id => {
                // Closing the window hides it; Quit is explicit from tray / UI.
                if let Some(w) = window.as_ref() {
                    w.set_visible(false);
                }
            }
            Event::Reopen {
                has_visible_windows,
                ..
            } => {
                if !has_visible_windows {
                    show_control_window(window.as_ref());
                }
            }
            Event::LoopDestroyed => {
                if let Ok(mut s) = supervisor.lock() {
                    let _ = s.stop();
                }
                webview.take();
                window.take();
                tray.take();
            }
            _ => {}
        }
    });
}

fn handle_ipc(
    msg: &str,
    supervisor: &Arc<Mutex<PanelSupervisor>>,
    password: &str,
    autostart_item: &CheckMenuItem,
    #[cfg(target_os = "macos")] hide_dock_item: &CheckMenuItem,
    prefs: &mut DesktopPrefs,
    paths: &paths::DesktopPaths,
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    elwt: &tao::event_loop::EventLoopWindowTarget<UserEvent>,
    control_flow: &mut ControlFlow,
    webview: Option<&wry::WebView>,
) {
    match msg {
        "open" => {
            let _ = open_url(PANEL_URL);
        }
        "start" => {
            if let Ok(mut s) = supervisor.lock() {
                if let Err(e) = s.start() {
                    eprintln!("start failed: {e:#}");
                }
            }
        }
        "stop" => {
            if let Ok(mut s) = supervisor.lock() {
                if let Err(e) = s.stop() {
                    eprintln!("stop failed: {e:#}");
                }
            }
        }
        "copy_password" => {
            if let Err(e) = copy_to_clipboard(password) {
                eprintln!("copy password failed: {e:#}");
            }
        }
        "update" => {
            let _ = open_url(stitch_bot::update::RELEASES_PAGE);
        }
        "quit" => {
            if let Ok(mut s) = supervisor.lock() {
                let _ = s.stop();
            }
            *control_flow = ControlFlow::Exit;
            return;
        }
        other if other.starts_with("toggle_autostart:") => {
            let enabled = other.ends_with(":1");
            if let Err(e) = autostart::set_enabled(enabled) {
                eprintln!("start at login failed: {e}");
                autostart_item.set_checked(!enabled);
            } else {
                autostart_item.set_checked(enabled);
            }
        }
        #[cfg(target_os = "macos")]
        other if other.starts_with("toggle_hide_dock:") => {
            let hide = other.ends_with(":1");
            prefs.hide_dock_icon = hide;
            hide_dock_item.set_checked(hide);
            if let Err(e) = prefs.save(paths) {
                eprintln!("saving desktop prefs failed: {e:#}");
            }
            apply_dock_policy(elwt, hide);
        }
        #[cfg(not(target_os = "macos"))]
        other if other.starts_with("toggle_hide_dock:") => {
            let _ = (other, &mut *prefs, paths);
        }
        _ => {}
    }
    sync_control_ui(webview, autostart_item, prefs.hide_dock_icon, supervisor);
}

fn sync_control_ui(
    webview: Option<&wry::WebView>,
    autostart_item: &CheckMenuItem,
    hide_dock: bool,
    supervisor: &Arc<Mutex<PanelSupervisor>>,
) {
    let Some(wv) = webview else { return };
    let panel_running = supervisor
        .lock()
        .map(|mut s| s.is_running())
        .unwrap_or(false);
    let script =
        control_ui::set_state_script(autostart_item.is_checked(), hide_dock, panel_running);
    if let Err(e) = wv.evaluate_script(&script) {
        eprintln!("stitch-desktop: updating control window failed: {e:#}");
    }
}

fn show_control_window(window: Option<&Window>) {
    if let Some(w) = window {
        w.set_visible(true);
        w.set_focus();
    }
}

#[cfg(target_os = "macos")]
fn apply_dock_policy(elwt: &tao::event_loop::EventLoopWindowTarget<UserEvent>, hide_dock: bool) {
    use tao::platform::macos::{ActivationPolicy, EventLoopWindowTargetExtMacOS};
    let policy = if hide_dock {
        ActivationPolicy::Accessory
    } else {
        ActivationPolicy::Regular
    };
    elwt.set_activation_policy_at_runtime(policy);
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

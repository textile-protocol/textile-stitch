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
mod keep_awake;
mod menu_icons;
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
use tray_icon::menu::{CheckMenuItem, IconMenuItem, Menu, MenuEvent, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::keep_awake::KeepAwakeController;
use crate::menu_icons::{ActionKind, MenuIcons, StatusKind};
use wry::WebViewBuilder;

use crate::prefs::DesktopPrefs;
use crate::supervise::PanelSupervisor;

const PANEL_URL: &str = "http://127.0.0.1:8420";

/// Public guide for always-on Linux hosts (EC2, VPS, bare metal).
const SERVER_INSTALL_DOCS: &str =
    "https://github.com/textile-protocol/textile-stitch/blob/main/docs/install-server.md";
const WINDOW_TITLE: &str = "Stitch";
const WINDOW_INNER_WIDTH: f64 = 380.0;
const WINDOW_INNER_HEIGHT: f64 = 520.0;

#[derive(Debug)]
enum UserEvent {
    Menu(tray_icon::menu::MenuId),
    Ipc(String),
    /// Periodic poll so the control window reflects panel exits without a click.
    RefreshStatus,
    /// Latest GitHub release newer than this build, or `None` when current / offline.
    UpdateCheckResult(Option<String>),
}

const STATUS_POLL_SECS: u64 = 2;
/// How often to re-query GitHub for a newer desktop release.
const UPDATE_POLL_SECS: u64 = 6 * 60 * 60;

fn main() {
    if let Err(e) = run() {
        let detail = format!("{e:#}");
        eprintln!("stitch-desktop: {detail}");
        // Finder launches have no terminal — surface fatal errors in a dialog
        // so "double-click → nothing" isn't the only feedback.
        show_launch_error(&detail);
        std::process::exit(1);
    }
}

/// Best-effort native alert when startup fails (macOS Finder / Dock launches).
fn show_launch_error(detail: &str) {
    #[cfg(target_os = "macos")]
    {
        // AppleScript string literals: escape `\`, `"`, and flatten newlines.
        let escaped: String = detail
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .chars()
            .map(|c| match c {
                '\n' | '\r' => ' ',
                other => other,
            })
            .collect();
        let script = format!(
            "display dialog \"Stitch couldn't start.\\n\\n{escaped}\" with title \"Stitch\" buttons {{\"OK\"}} default button \"OK\" with icon stop"
        );
        let _ = std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .status();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = detail;
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
    let mut prefs = DesktopPrefs::load(&paths);
    // First run (or legacy cleartext `panel.password`): user picks a password
    // before the panel starts. Hash-only on disk — no copy-to-clipboard path.
    let mut awaiting_signup = password::needs_setup(&paths);
    let legacy_password_reset = paths.password_file.exists();

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
    let update_proxy = proxy.clone();
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
    // Best-effort release check (no install receipt needed — works for Stitch.app).
    std::thread::spawn(move || loop {
        let latest = stitch_bot::update::newer_release_blocking();
        if update_proxy
            .send_event(UserEvent::UpdateCheckResult(latest))
            .is_err()
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(UPDATE_POLL_SECS));
    });

    let menu_icons = MenuIcons::new();
    let panel_running = supervisor
        .lock()
        .map(|mut s| s.is_running())
        .unwrap_or(false);

    let mut keep_awake = KeepAwakeController::new();
    if prefs.keep_awake {
        if let Err(e) = keep_awake.set_enabled(true) {
            eprintln!("stitch-desktop: keep awake on launch failed: {e:#}");
            prefs.keep_awake = false;
            let _ = prefs.save(&paths);
        }
    }

    // Tray menu: status, open/pause, keep awake, updates, settings, quit.
    // Login / Dock prefs live only in the Settings window; keep-awake is in
    // both (quick toggle from the tray, like Amphetamine).
    let status_item = menu_icons::status_item(
        if panel_running {
            "Panel running"
        } else {
            "Panel stopped"
        },
        if panel_running {
            StatusKind::Running
        } else {
            StatusKind::Stopped
        },
        &menu_icons,
    );
    let open_item = menu_icons::action_item("Open Stitch panel", ActionKind::Open, &menu_icons);
    let pause_item = menu_icons::action_item(
        if panel_running { "Pause" } else { "Resume" },
        if panel_running {
            ActionKind::Pause
        } else {
            ActionKind::Resume
        },
        &menu_icons,
    );
    let keep_awake_item = CheckMenuItem::new(keep_awake::label(), true, prefs.keep_awake, None);
    let update_item =
        menu_icons::action_item("Check for updates…", ActionKind::Update, &menu_icons);
    let settings_item = menu_icons::action_item("Settings", ActionKind::Show, &menu_icons);
    let quit_item = menu_icons::action_item("Quit Stitch", ActionKind::Quit, &menu_icons);

    let open_id = open_item.id().clone();
    let pause_id = pause_item.id().clone();
    let keep_awake_id = keep_awake_item.id().clone();
    let update_id = update_item.id().clone();
    let settings_id = settings_item.id().clone();
    let quit_id = quit_item.id().clone();

    let menu = Menu::new();
    menu.append(&status_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&open_item)?;
    menu.append(&pause_item)?;
    menu.append(&keep_awake_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&update_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&settings_item)?;
    menu.append(&quit_item)?;
    // Password setup always shows the window, even on login-item autostart.
    let show_window = !quiet_launch || awaiting_signup;
    let window = WindowBuilder::new()
        .with_title(WINDOW_TITLE)
        .with_inner_size(tao::dpi::LogicalSize::new(
            WINDOW_INNER_WIDTH,
            WINDOW_INNER_HEIGHT,
        ))
        .with_visible(show_window)
        .build(&event_loop)
        .context("creating Stitch window")?;
    let window_id = window.id();

    #[cfg(target_os = "macos")]
    let hide_dock_row = true;
    #[cfg(not(target_os = "macos"))]
    let hide_dock_row = false;

    // Cache OS login-item state. RefreshStatus runs every few seconds; on Windows
    // is_enabled() shells out to `reg query`, which stalls the UI thread.
    let mut autostart_enabled = autostart::is_enabled();

    let html = if awaiting_signup {
        control_ui::signup_html(legacy_password_reset)
    } else {
        control_ui::html(
            autostart_enabled,
            prefs.hide_dock_icon,
            prefs.keep_awake,
            panel_running,
            hide_dock_row,
        )
    };
    let webview = WebViewBuilder::new()
        .with_html(&html)
        .with_ipc_handler(move |req| {
            let _ = ipc_proxy.send_event(UserEvent::Ipc(req.body().to_string()));
        })
        .build(&window)
        .context("creating Stitch control webview")?;

    // tray-icon requires the macOS event loop to be running before TrayIcon::new.
    let mut tray: Option<TrayIcon> = None;
    let mut started_panel = false;
    let mut update_version: Option<String> = None;
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
                    let icon = tray_icon_for_state(prefs.keep_awake);
                    let tooltip = tray_tooltip(prefs.keep_awake);
                    let tray_builder = TrayIconBuilder::new()
                        .with_menu(Box::new(menu.clone()))
                        .with_tooltip(tooltip)
                        .with_icon(icon);
                    // macOS menu bar: template image so the system tints the
                    // monochrome grandma for light/dark menu bar chrome.
                    #[cfg(target_os = "macos")]
                    let tray_builder = tray_builder.with_icon_as_template(true);
                    match tray_builder.build() {
                        Ok(t) => tray = Some(t),
                        Err(e) => {
                            eprintln!("stitch-desktop: creating menu bar icon failed: {e:#}");
                            *control_flow = ControlFlow::Exit;
                            return;
                        }
                    }
                }
                if !started_panel && !awaiting_signup {
                    started_panel = true;
                    start_panel_and_maybe_open(
                        &supervisor,
                        !quiet_launch,
                        webview.as_ref(),
                        autostart_enabled,
                        prefs.hide_dock_icon,
                        prefs.keep_awake,
                        update_version.as_deref(),
                    );
                }
            }
            Event::UserEvent(UserEvent::Menu(id)) => {
                if id == settings_id {
                    show_control_window(window.as_ref());
                } else if id == open_id {
                    if awaiting_signup {
                        show_control_window(window.as_ref());
                    } else {
                        let _ = open_url(PANEL_URL);
                    }
                } else if id == pause_id {
                    if awaiting_signup {
                        show_control_window(window.as_ref());
                    } else {
                        toggle_panel(&supervisor);
                        sync_tray_and_window(
                            &status_item,
                            &pause_item,
                            &update_item,
                            &menu_icons,
                            webview.as_ref(),
                            autostart_enabled,
                            prefs.hide_dock_icon,
                            prefs.keep_awake,
                            &supervisor,
                            update_version.as_deref(),
                        );
                    }
                } else if id == keep_awake_id {
                    // muda toggles the checkmark before we see the event.
                    let enabled = keep_awake_item.is_checked();
                    apply_keep_awake(
                        enabled,
                        &mut keep_awake,
                        &mut prefs,
                        &paths,
                        &keep_awake_item,
                        tray.as_ref(),
                    );
                    if !awaiting_signup {
                        sync_control_ui(
                            webview.as_ref(),
                            autostart_enabled,
                            prefs.hide_dock_icon,
                            prefs.keep_awake,
                            &supervisor,
                            update_version.as_deref(),
                        );
                    }
                } else if id == update_id {
                    let _ = open_url(stitch_bot::update::RELEASES_PAGE);
                } else if id == quit_id {
                    if let Ok(mut s) = supervisor.lock() {
                        let _ = s.stop();
                    }
                    let _ = keep_awake.set_enabled(false);
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Ipc(msg)) => {
                if awaiting_signup {
                    if let Some((pw, confirm)) = parse_signup_ipc(&msg) {
                        match password::set_panel_password(&paths, &pw, &confirm) {
                            Ok(()) => {
                                awaiting_signup = false;
                                let control_html = control_ui::html(
                                    autostart_enabled,
                                    prefs.hide_dock_icon,
                                    prefs.keep_awake,
                                    false,
                                    hide_dock_row,
                                );
                                if let Some(wv) = webview.as_ref() {
                                    if let Err(e) = wv.load_html(&control_html) {
                                        eprintln!(
                                            "stitch-desktop: loading control window failed: {e:#}"
                                        );
                                    }
                                }
                                show_control_window(window.as_ref());
                                if !started_panel {
                                    started_panel = true;
                                    start_panel_and_maybe_open(
                                        &supervisor,
                                        true,
                                        webview.as_ref(),
                                        autostart_enabled,
                                        prefs.hide_dock_icon,
                                        prefs.keep_awake,
                                        update_version.as_deref(),
                                    );
                                }
                            }
                            Err(e) => {
                                if let Some(wv) = webview.as_ref() {
                                    let script = control_ui::signup_error_script(&format!("{e:#}"));
                                    let _ = wv.evaluate_script(&script);
                                }
                            }
                        }
                    }
                } else {
                    handle_ipc(
                        &msg,
                        &supervisor,
                        &mut prefs,
                        &paths,
                        elwt,
                        control_flow,
                        webview.as_ref(),
                        &mut autostart_enabled,
                        &mut keep_awake,
                        &keep_awake_item,
                        tray.as_ref(),
                        update_version.as_deref(),
                    );
                }
            }
            Event::UserEvent(UserEvent::UpdateCheckResult(latest)) => {
                update_version = latest;
                sync_tray_menu(
                    &status_item,
                    &pause_item,
                    &update_item,
                    &menu_icons,
                    &supervisor,
                    update_version.as_deref(),
                );
                if !awaiting_signup {
                    sync_control_ui(
                        webview.as_ref(),
                        autostart_enabled,
                        prefs.hide_dock_icon,
                        prefs.keep_awake,
                        &supervisor,
                        update_version.as_deref(),
                    );
                }
            }
            Event::UserEvent(UserEvent::RefreshStatus) => {
                sync_tray_menu(
                    &status_item,
                    &pause_item,
                    &update_item,
                    &menu_icons,
                    &supervisor,
                    update_version.as_deref(),
                );
                if !awaiting_signup {
                    sync_control_ui(
                        webview.as_ref(),
                        autostart_enabled,
                        prefs.hide_dock_icon,
                        prefs.keep_awake,
                        &supervisor,
                        update_version.as_deref(),
                    );
                }
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
                let _ = keep_awake.set_enabled(false);
                webview.take();
                window.take();
                tray.take();
            }
            _ => {}
        }
    });
}

fn toggle_panel(supervisor: &Arc<Mutex<PanelSupervisor>>) {
    if let Ok(mut s) = supervisor.lock() {
        if s.is_running() {
            if let Err(e) = s.stop() {
                eprintln!("pause failed: {e:#}");
            }
        } else if let Err(e) = s.start() {
            eprintln!("resume failed: {e:#}");
        }
    }
}

fn sync_tray_menu(
    status_item: &IconMenuItem,
    pause_item: &IconMenuItem,
    update_item: &IconMenuItem,
    icons: &MenuIcons,
    supervisor: &Arc<Mutex<PanelSupervisor>>,
    update_version: Option<&str>,
) {
    let running = supervisor
        .lock()
        .map(|mut s| s.is_running())
        .unwrap_or(false);
    if running {
        status_item.set_text("Panel running");
        menu_icons::apply_status(status_item, StatusKind::Running, icons);
        pause_item.set_text("Pause");
        menu_icons::apply_action(pause_item, ActionKind::Pause, icons);
    } else {
        status_item.set_text("Panel stopped");
        menu_icons::apply_status(status_item, StatusKind::Stopped, icons);
        pause_item.set_text("Resume");
        menu_icons::apply_action(pause_item, ActionKind::Resume, icons);
    }
    if update_version.is_some() {
        update_item.set_text("Download update");
    } else {
        update_item.set_text("Check for updates…");
    }
}

fn sync_tray_and_window(
    status_item: &IconMenuItem,
    pause_item: &IconMenuItem,
    update_item: &IconMenuItem,
    icons: &MenuIcons,
    webview: Option<&wry::WebView>,
    autostart: bool,
    hide_dock: bool,
    keep_awake: bool,
    supervisor: &Arc<Mutex<PanelSupervisor>>,
    update_version: Option<&str>,
) {
    sync_tray_menu(
        status_item,
        pause_item,
        update_item,
        icons,
        supervisor,
        update_version,
    );
    sync_control_ui(
        webview,
        autostart,
        hide_dock,
        keep_awake,
        supervisor,
        update_version,
    );
}

fn start_panel_and_maybe_open(
    supervisor: &Arc<Mutex<PanelSupervisor>>,
    open_browser: bool,
    webview: Option<&wry::WebView>,
    autostart: bool,
    hide_dock: bool,
    keep_awake: bool,
    update_version: Option<&str>,
) {
    let start_result = {
        let mut s = supervisor.lock().unwrap();
        s.start().context("starting the local Stitch panel")
    };
    match start_result {
        Ok(()) => {
            if open_browser {
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
        webview,
        autostart,
        hide_dock,
        keep_awake,
        supervisor,
        update_version,
    );
}

fn parse_signup_ipc(msg: &str) -> Option<(String, String)> {
    #[derive(serde::Deserialize)]
    struct SignupMsg {
        op: String,
        password: String,
        confirm: String,
    }
    let parsed: SignupMsg = serde_json::from_str(msg).ok()?;
    if parsed.op != "signup" {
        return None;
    }
    Some((parsed.password, parsed.confirm))
}

fn handle_ipc(
    msg: &str,
    supervisor: &Arc<Mutex<PanelSupervisor>>,
    prefs: &mut DesktopPrefs,
    paths: &paths::DesktopPaths,
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    elwt: &tao::event_loop::EventLoopWindowTarget<UserEvent>,
    control_flow: &mut ControlFlow,
    webview: Option<&wry::WebView>,
    autostart_enabled: &mut bool,
    keep_awake: &mut KeepAwakeController,
    keep_awake_item: &CheckMenuItem,
    tray: Option<&TrayIcon>,
    update_version: Option<&str>,
) {
    match msg {
        "open" => {
            let _ = open_url(PANEL_URL);
        }
        "open_server_docs" => {
            let _ = open_url(SERVER_INSTALL_DOCS);
        }
        "toggle_panel" => {
            toggle_panel(supervisor);
        }
        "update" => {
            let _ = open_url(stitch_bot::update::RELEASES_PAGE);
        }
        "quit" => {
            if let Ok(mut s) = supervisor.lock() {
                let _ = s.stop();
            }
            let _ = keep_awake.set_enabled(false);
            *control_flow = ControlFlow::Exit;
            return;
        }
        other if other.starts_with("toggle_autostart:") => {
            let enabled = other.ends_with(":1");
            match autostart::set_enabled(enabled) {
                Ok(()) => *autostart_enabled = enabled,
                Err(e) => eprintln!("start at login failed: {e}"),
            }
        }
        other if other.starts_with("toggle_keep_awake:") => {
            let enabled = other.ends_with(":1");
            apply_keep_awake(enabled, keep_awake, prefs, paths, keep_awake_item, tray);
        }
        #[cfg(target_os = "macos")]
        other if other.starts_with("toggle_hide_dock:") => {
            let hide = other.ends_with(":1");
            prefs.hide_dock_icon = hide;
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
    sync_control_ui(
        webview,
        *autostart_enabled,
        prefs.hide_dock_icon,
        prefs.keep_awake,
        supervisor,
        update_version,
    );
}

fn apply_keep_awake(
    enabled: bool,
    controller: &mut KeepAwakeController,
    prefs: &mut DesktopPrefs,
    paths: &paths::DesktopPaths,
    menu_item: &CheckMenuItem,
    tray: Option<&TrayIcon>,
) {
    match controller.set_enabled(enabled) {
        Ok(()) => {
            prefs.keep_awake = enabled;
            if let Err(e) = prefs.save(paths) {
                eprintln!("saving desktop prefs failed: {e:#}");
            }
            menu_item.set_checked(enabled);
            apply_tray_keep_awake_chrome(tray, enabled);
        }
        Err(e) => {
            eprintln!("keep awake failed: {e:#}");
            prefs.keep_awake = false;
            let _ = controller.set_enabled(false);
            menu_item.set_checked(false);
            apply_tray_keep_awake_chrome(tray, false);
            if let Err(save_err) = prefs.save(paths) {
                eprintln!("saving desktop prefs failed: {save_err:#}");
            }
        }
    }
}

fn apply_tray_keep_awake_chrome(tray: Option<&TrayIcon>, keep_awake: bool) {
    let Some(tray) = tray else { return };
    if let Err(e) = tray.set_icon(Some(tray_icon_for_state(keep_awake))) {
        eprintln!("stitch-desktop: updating tray icon failed: {e:#}");
    }
    if let Err(e) = tray.set_tooltip(Some(tray_tooltip(keep_awake))) {
        eprintln!("stitch-desktop: updating tray tooltip failed: {e:#}");
    }
}

fn sync_control_ui(
    webview: Option<&wry::WebView>,
    autostart: bool,
    hide_dock: bool,
    keep_awake: bool,
    supervisor: &Arc<Mutex<PanelSupervisor>>,
    update_version: Option<&str>,
) {
    let Some(wv) = webview else { return };
    let panel_running = supervisor
        .lock()
        .map(|mut s| s.is_running())
        .unwrap_or(false);
    let script = control_ui::set_state_script(
        autostart,
        hide_dock,
        keep_awake,
        panel_running,
        update_version,
    );
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

fn tray_tooltip(keep_awake: bool) -> &'static str {
    if keep_awake {
        "Stitch — keeping awake"
    } else {
        "Stitch"
    }
}

fn tray_icon_for_state(keep_awake: bool) -> Icon {
    if keep_awake {
        tray_icon_with_awake_badge().unwrap_or_else(fallback_icon)
    } else {
        tray_icon_from_embedded().unwrap_or_else(fallback_icon)
    }
}

fn tray_icon_from_embedded() -> Option<Icon> {
    // Monochrome geometric grandma (32×32 premultiplied-ready RGBA, black + alpha).
    // Built as a menu-bar / tray template: macOS tints it; Windows/Linux show black.
    const SIZE: u32 = 32;
    const RGBA: &[u8] = include_bytes!("../../../assets/grandma-tray-32.rgba");
    if RGBA.len() != (SIZE * SIZE * 4) as usize {
        return None;
    }
    Icon::from_rgba(RGBA.to_vec(), SIZE, SIZE).ok()
}

/// Grandma tray icon plus a small lightning badge in the lower-right corner.
/// Template-safe (black + alpha) so macOS menu-bar tinting still works.
fn tray_icon_with_awake_badge() -> Option<Icon> {
    const SIZE: u32 = 32;
    const RGBA: &[u8] = include_bytes!("../../../assets/grandma-tray-32.rgba");
    if RGBA.len() != (SIZE * SIZE * 4) as usize {
        return None;
    }
    let mut px = RGBA.to_vec();
    paint_awake_badge(&mut px, SIZE);
    Icon::from_rgba(px, SIZE, SIZE).ok()
}

/// Draw a compact lightning bolt (template ink) into the lower-right of a
/// 32×32 RGBA buffer. Cleared with a soft hole first so it reads as an overlay
/// even when the grandma mark already occupies that corner.
fn paint_awake_badge(rgba: &mut [u8], size: u32) {
    // Bolt polygon in icon space (lower-right).
    let bolt: [(f32, f32); 6] = [
        (22.0, 17.0),
        (27.5, 17.0),
        (24.5, 22.0),
        (29.0, 22.0),
        (21.5, 30.5),
        (23.5, 23.5),
    ];
    // Soft circular wipe behind the bolt so it doesn't merge into grandma ink.
    for y in 16..size as i32 {
        for x in 20..size as i32 {
            let dx = x as f32 + 0.5 - 25.5;
            let dy = y as f32 + 0.5 - 24.0;
            if dx * dx + dy * dy <= 7.5 * 7.5 {
                let i = ((y as u32 * size + x as u32) * 4) as usize;
                rgba[i] = 0;
                rgba[i + 1] = 0;
                rgba[i + 2] = 0;
                rgba[i + 3] = 0;
            }
        }
    }
    for y in 0..size as i32 {
        for x in 0..size as i32 {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            if point_in_polygon(px, py, &bolt) {
                let i = ((y as u32 * size + x as u32) * 4) as usize;
                rgba[i] = 0;
                rgba[i + 1] = 0;
                rgba[i + 2] = 0;
                rgba[i + 3] = 255;
            }
        }
    }
}

fn point_in_polygon(x: f32, y: f32, poly: &[(f32, f32)]) -> bool {
    // Ray cast — even-odd fill for the small badge polygon.
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        let intersect =
            ((yi > y) != (yj > y)) && (x < (xj - xi) * (y - yi) / (yj - yi + f32::EPSILON) + xi);
        if intersect {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn fallback_icon() -> Icon {
    tray_icon_from_embedded().expect("grandma tray icon")
}

#[cfg(test)]
mod tray_badge_tests {
    use super::paint_awake_badge;

    #[test]
    fn awake_badge_sets_ink_pixels() {
        let mut rgba = vec![0u8; 32 * 32 * 4];
        paint_awake_badge(&mut rgba, 32);
        let ink = rgba.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert!(ink > 10, "expected lightning ink, got {ink} pixels");
        assert!(
            ink < 120,
            "badge should stay a small corner overlay, got {ink}"
        );
    }
}

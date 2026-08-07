// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Stitch desktop — menu bar / system tray controller with a Dock icon and
//! control window (macOS Dock can be hidden via a preference).
//!
//! Starts `stitch-panel` in process runtime (no Docker) and offers
//! start/stop/update without a terminal. The browser UI is the same Stitch
//! panel used on servers; open it from the control window or tray when ready.
//! The desktop window mirrors tray actions.
//!
//! Pass `--autostart` (set by the OS login item) to skip showing the control
//! window; the panel still starts and restores bots that were left running.
//! Interactive launches also leave the browser closed until the operator clicks
//! **Open Stitch panel** (avoids a connection-refused flash while the panel
//! comes up, and skips a console flash from `cmd start` on Windows).
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
mod update_install;
mod win_cmd;
#[cfg(windows)]
mod win_reg;

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use stitch_bot::update::{ReleaseAsset, ReleaseCheck};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::{Window, WindowBuilder};
use tray_icon::menu::{IconMenuItem, Menu, MenuEvent, PredefinedMenuItem};
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
const PAUSE_CONFIRMATION_TITLE: &str = "Pause Stitch?";
const PAUSE_CONFIRMATION_MESSAGE: &str = "Pausing the panel also pauses every bot that is running now. When you resume, Stitch restarts only those bots. Bots that were already paused stay paused.";
/// Note set while a tray/Settings "Check for updates…" is in flight. Background
/// polls leave this unset so they stay quiet when current or when an update
/// appears.
const CHECKING_UPDATES_NOTE: &str = "Checking for updates…";
const UPDATE_AVAILABLE_TITLE: &str = "Update available";
const UPDATE_UPGRADE_BUTTON: &str = "Upgrade";
const UPDATE_LATER_BUTTON: &str = "Later";

#[derive(Debug)]
enum UserEvent {
    Menu(tray_icon::menu::MenuId),
    Ipc(String),
    /// Periodic poll so the control window reflects panel exits without a click.
    RefreshStatus,
    /// Outcome of a GitHub latest-release poll (background or manual).
    /// `manual` is true only for checks woken by "Check for updates…", not
    /// inferred from UI note text (an in-flight background poll must not be
    /// treated as the kicked check).
    UpdateCheckResult {
        result: ReleaseCheck,
        manual: bool,
    },
    /// Outcome of downloading and verifying the selected platform artifact.
    UpdateDownloadResult(Result<PathBuf, String>),
}

const STATUS_POLL_SECS: u64 = 2;
/// How often to re-query GitHub once we know the network works.
const UPDATE_POLL_SECS: u64 = 6 * 60 * 60;
/// Backoff steps when a release check fails (wifi/DNS race on launch).
const UPDATE_RETRY_SECS: &[u64] = &[30, 120, 600, UPDATE_POLL_SECS];

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

/// Best-effort native alert when startup fails (no terminal for GUI launches).
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
    #[cfg(target_os = "windows")]
    {
        // PowerShell MessageBox — no extra crate; hide the console host.
        let escaped = detail
            .replace('\'', "''")
            .chars()
            .map(|c| match c {
                '\n' | '\r' => ' ',
                other => other,
            })
            .collect::<String>();
        let script = format!(
            "Add-Type -AssemblyName PresentationFramework; \
             [System.Windows.MessageBox]::Show(\
               'Stitch couldn''t start.`n`n{escaped}',\
               'Stitch','OK','Error') | Out-Null"
        );
        let mut cmd = std::process::Command::new("powershell");
        cmd.args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script]);
        win_cmd::no_window(&mut cmd);
        let _ = cmd.status();
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let _ = detail;
    }
}

/// Escape a string for embedding in an AppleScript double-quoted literal.
#[cfg(target_os = "macos")]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .chars()
        .map(|c| match c {
            '\n' | '\r' => ' ',
            other => other,
        })
        .collect()
}

/// Escape a string for embedding in a PowerShell single-quoted literal.
#[cfg(target_os = "windows")]
fn powershell_single_quote(s: &str) -> String {
    s.replace('\'', "''")
        .chars()
        .map(|c| match c {
            '\n' | '\r' => ' ',
            other => other,
        })
        .collect()
}

/// After a manual "Check for updates…", ask whether to open the install flow.
/// Returns true when the operator chooses Upgrade.
fn confirm_update_available(version: &str) -> bool {
    let message = format!("Stitch {version} is ready.");
    #[cfg(target_os = "macos")]
    {
        let title = applescript_escape(UPDATE_AVAILABLE_TITLE);
        let body = applescript_escape(&message);
        let upgrade = applescript_escape(UPDATE_UPGRADE_BUTTON);
        let later = applescript_escape(UPDATE_LATER_BUTTON);
        let script = format!(
            "display alert \"{title}\" message \"{body}\" buttons {{\"{later}\", \"{upgrade}\"}} default button \"{upgrade}\" cancel button \"{later}\""
        );
        return std::process::Command::new("/usr/bin/osascript")
            .args(["-e", &script])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    }
    #[cfg(target_os = "windows")]
    {
        // MessageBox has fixed Yes/No labels; map Yes → Upgrade, No → Later.
        let title = powershell_single_quote(UPDATE_AVAILABLE_TITLE);
        let body = powershell_single_quote(&format!(
            "{message}`n`nClick Yes to upgrade, or No to decide later."
        ));
        let script = format!(
            "Add-Type -AssemblyName PresentationFramework; \
             $answer = [System.Windows.MessageBox]::Show(\
               '{body}',\
               '{title}','YesNo','Information'); \
             if ($answer -eq 'Yes') {{ exit 0 }} else {{ exit 1 }}"
        );
        let mut cmd = std::process::Command::new("powershell");
        cmd.args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script]);
        win_cmd::no_window(&mut cmd);
        return cmd.status().map(|status| status.success()).unwrap_or(false);
    }
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::*;
        let dialog = gtk::MessageDialog::new(
            None::<&gtk::Window>,
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Info,
            gtk::ButtonsType::None,
            &message,
        );
        dialog.set_title(UPDATE_AVAILABLE_TITLE);
        dialog.add_button(UPDATE_LATER_BUTTON, gtk::ResponseType::Cancel);
        dialog.add_button(UPDATE_UPGRADE_BUTTON, gtk::ResponseType::Accept);
        dialog.set_default_response(gtk::ResponseType::Accept);
        let confirmed = dialog.run() == gtk::ResponseType::Accept;
        dialog.close();
        return confirmed;
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = message;
        false
    }
}

/// Confirm the fleet-wide effect before pausing the panel. Fail closed when a
/// platform dialog cannot be shown: silently stopping trading bots is worse
/// than leaving the panel running.
fn confirm_panel_pause() -> bool {
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display alert \"{PAUSE_CONFIRMATION_TITLE}\" message \"{PAUSE_CONFIRMATION_MESSAGE}\" buttons {{\"Cancel\", \"Pause panel and bots\"}} default button \"Pause panel and bots\" cancel button \"Cancel\" as warning"
        );
        return std::process::Command::new("/usr/bin/osascript")
            .args(["-e", &script])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    }
    #[cfg(target_os = "windows")]
    {
        let script = format!(
            "Add-Type -AssemblyName PresentationFramework; \
             $answer = [System.Windows.MessageBox]::Show(\
               '{PAUSE_CONFIRMATION_MESSAGE}',\
               '{PAUSE_CONFIRMATION_TITLE}','OKCancel','Warning'); \
             if ($answer -eq 'OK') {{ exit 0 }} else {{ exit 1 }}"
        );
        let mut cmd = std::process::Command::new("powershell");
        cmd.args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script]);
        win_cmd::no_window(&mut cmd);
        return cmd.status().map(|status| status.success()).unwrap_or(false);
    }
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::*;
        let dialog = gtk::MessageDialog::new(
            None::<&gtk::Window>,
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Warning,
            gtk::ButtonsType::OkCancel,
            PAUSE_CONFIRMATION_MESSAGE,
        );
        dialog.set_title(PAUSE_CONFIRMATION_TITLE);
        let confirmed = dialog.run() == gtk::ResponseType::Ok;
        dialog.close();
        return confirmed;
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        false
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
    let download_proxy = proxy.clone();
    let (update_kick_tx, update_kick_rx) = mpsc::channel::<()>();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = proxy.send_event(UserEvent::Menu(event.id));
    }));
    // The event loop is Wait-based — without a timer, a crashed panel leaves the
    // control window stuck on "Panel running" until the next user action.
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(STATUS_POLL_SECS));
        if status_proxy.send_event(UserEvent::RefreshStatus).is_err() {
            break;
        }
    });
    // Best-effort release check (no install receipt needed — works for Stitch.app).
    // Failed polls retry quickly; successful Current/Available settle on the
    // long interval. A kick from "Check for updates…" runs immediately and is
    // tagged `manual` on that next poll only — never on an already in-flight one.
    std::thread::spawn(move || {
        let mut fail_streak: usize = 0;
        let mut manual = false;
        loop {
            let result = stitch_bot::update::check_latest_release_blocking();
            let sleep_secs = match &result {
                ReleaseCheck::Failed { reason } => {
                    let idx = fail_streak.min(UPDATE_RETRY_SECS.len() - 1);
                    fail_streak = fail_streak.saturating_add(1);
                    eprintln!(
                        "stitch-desktop: update check failed ({reason}); retry in {}s",
                        UPDATE_RETRY_SECS[idx]
                    );
                    UPDATE_RETRY_SECS[idx]
                }
                ReleaseCheck::Available { .. } | ReleaseCheck::Current => {
                    fail_streak = 0;
                    UPDATE_POLL_SECS
                }
            };
            if update_proxy
                .send_event(UserEvent::UpdateCheckResult { result, manual })
                .is_err()
            {
                break;
            }
            manual = false;
            // Wait for the interval, or wake early on a manual check kick.
            match update_kick_rx.recv_timeout(Duration::from_secs(sleep_secs)) {
                Ok(()) => {
                    // Collapse duplicate kicks into one tagged check.
                    while update_kick_rx.try_recv().is_ok() {}
                    manual = true;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    let mut menu_icons = MenuIcons::new();
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
    let keep_awake_item = menu_icons::keep_awake_item(prefs.keep_awake, &menu_icons);
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

    // Cache OS login-item state (toggle updates this; avoid re-querying every poll).
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
            None,
            None,
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
    let mut update_asset: Option<ReleaseAsset> = None;
    let mut update_note: Option<String> = None;
    let mut update_in_progress = false;
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
                    // The normal grandma is a macOS template. The awake state
                    // carries a yellow dot, so it is an appearance-aware color
                    // bitmap and must not be system-tinted.
                    #[cfg(target_os = "macos")]
                    let tray_builder = tray_builder.with_icon_as_template(!prefs.keep_awake);
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
                    start_panel(
                        &supervisor,
                        webview.as_ref(),
                        autostart_enabled,
                        prefs.hide_dock_icon,
                        prefs.keep_awake,
                        update_version.as_deref(),
                        update_note.as_deref(),
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
                            update_note.as_deref(),
                        );
                    }
                } else if id == keep_awake_id {
                    // IconMenuItem (needed for the computer glyph) does not
                    // auto-toggle — flip from current prefs.
                    let enabled = !prefs.keep_awake;
                    apply_keep_awake(
                        enabled,
                        &mut keep_awake,
                        &mut prefs,
                        &paths,
                        &keep_awake_item,
                        &menu_icons,
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
                            update_note.as_deref(),
                        );
                    }
                } else if id == update_id {
                    handle_update_action(
                        &update_kick_tx,
                        &mut update_version,
                        &mut update_note,
                        webview.as_ref(),
                        autostart_enabled,
                        prefs.hide_dock_icon,
                        prefs.keep_awake,
                        &supervisor,
                        &status_item,
                        &pause_item,
                        &update_item,
                        &menu_icons,
                        window.as_ref(),
                    );
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
                                // Start the panel *before* navigating away from
                                // signup HTML, then bake any failure into the
                                // control document. evaluate_script right after
                                // load_html is a no-op while the old page is
                                // still active (no __stitchPanelError yet).
                                let (panel_running_now, panel_err) = if !started_panel {
                                    started_panel = true;
                                    match try_start_panel(&supervisor) {
                                        Ok(()) => (true, None),
                                        Err(e) => {
                                            eprintln!("stitch-desktop: {e:#}");
                                            (false, Some(format!("{e:#}")))
                                        }
                                    }
                                } else {
                                    let running = supervisor
                                        .lock()
                                        .map(|mut s| s.is_running())
                                        .unwrap_or(false);
                                    (running, None)
                                };
                                let control_html = control_ui::html(
                                    autostart_enabled,
                                    prefs.hide_dock_icon,
                                    prefs.keep_awake,
                                    panel_running_now,
                                    hide_dock_row,
                                    panel_err.as_deref(),
                                    update_version.as_deref(),
                                );
                                if let Some(wv) = webview.as_ref() {
                                    if let Err(e) = wv.load_html(&control_html) {
                                        eprintln!(
                                            "stitch-desktop: loading control window failed: {e:#}"
                                        );
                                    }
                                }
                                show_control_window(window.as_ref());
                                sync_control_ui(
                                    webview.as_ref(),
                                    autostart_enabled,
                                    prefs.hide_dock_icon,
                                    prefs.keep_awake,
                                    &supervisor,
                                    update_version.as_deref(),
                                    update_note.as_deref(),
                                );
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
                        &menu_icons,
                        tray.as_ref(),
                        &update_kick_tx,
                        &mut update_version,
                        update_asset.as_ref(),
                        &mut update_note,
                        &mut update_in_progress,
                        &download_proxy,
                        &status_item,
                        &pause_item,
                        &update_item,
                    );
                }
            }
            Event::UserEvent(UserEvent::UpdateCheckResult { result, manual }) => {
                let prompt_version = apply_release_check(
                    &mut update_version,
                    &mut update_asset,
                    &mut update_note,
                    result,
                    manual,
                );
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
                        update_note.as_deref(),
                    );
                }
                // Manual "Check for updates…" found a release — ask before
                // opening the Settings install dialog. Skip during password
                // setup: the signup document has no update dialog. Background
                // polls only flip the tray to Update.
                if !awaiting_signup {
                    if let Some(version) = prompt_version {
                        if confirm_update_available(&version) {
                            show_control_window(window.as_ref());
                            if let Some(wv) = webview.as_ref() {
                                let _ = wv.evaluate_script(control_ui::show_update_dialog_script());
                            }
                        }
                    }
                }
            }
            Event::UserEvent(UserEvent::UpdateDownloadResult(result)) => {
                update_in_progress = false;
                match result {
                    Ok(path) => match update_install::stage(&path) {
                        Ok(()) => {
                            if let Ok(mut s) = supervisor.lock() {
                                let _ = s.stop();
                            }
                            let _ = keep_awake.set_enabled(false);
                            *control_flow = ControlFlow::Exit;
                        }
                        Err(error) => {
                            update_note = Some(format!("Couldn't install update: {error:#}"));
                        }
                    },
                    Err(error) => {
                        update_note = Some(format!("Couldn't download update: {error}"));
                    }
                }
                if !matches!(*control_flow, ControlFlow::Exit) {
                    sync_control_ui(
                        webview.as_ref(),
                        autostart_enabled,
                        prefs.hide_dock_icon,
                        prefs.keep_awake,
                        &supervisor,
                        update_version.as_deref(),
                        update_note.as_deref(),
                    );
                }
            }
            Event::UserEvent(UserEvent::RefreshStatus) => {
                // Custom menu icons bake OS ink into RGBA. muda can't mark them
                // as AppKit templates, so rebuild when light/dark flips.
                if menu_icons.refresh_for_appearance() {
                    let running = supervisor
                        .lock()
                        .map(|mut s| s.is_running())
                        .unwrap_or(false);
                    menu_icons::reapply_all(
                        &menu_icons,
                        &status_item,
                        if running {
                            StatusKind::Running
                        } else {
                            StatusKind::Stopped
                        },
                        &open_item,
                        &pause_item,
                        if running {
                            ActionKind::Pause
                        } else {
                            ActionKind::Resume
                        },
                        &keep_awake_item,
                        prefs.keep_awake,
                        &update_item,
                        &settings_item,
                        &quit_item,
                    );
                    if prefs.keep_awake {
                        apply_tray_keep_awake_chrome(tray.as_ref(), true);
                    }
                }
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
                        update_note.as_deref(),
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
    // Sample running state under a short lock, then drop it before any
    // blocking confirmation. On Linux, gtk::Dialog::run() nests the GTK loop
    // and can dispatch RefreshStatus, which also needs this mutex — holding
    // it across the dialog deadlocks the UI thread.
    let running = match supervisor.lock() {
        Ok(mut s) => s.is_running(),
        Err(_) => return,
    };

    if running {
        if !confirm_panel_pause() {
            return;
        }
        if let Ok(mut s) = supervisor.lock() {
            // State may have changed while the dialog was open.
            if !s.is_running() {
                return;
            }
            if let Err(e) = s.stop() {
                eprintln!("pause failed: {e:#}");
            }
        }
        return;
    }

    if let Ok(mut s) = supervisor.lock() {
        if let Err(e) = s.start() {
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
        update_item.set_text("Update");
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
    update_note: Option<&str>,
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
        update_note,
    );
}

fn try_start_panel(supervisor: &Arc<Mutex<PanelSupervisor>>) -> Result<()> {
    let mut s = supervisor.lock().unwrap();
    s.start().context("starting the local Stitch panel")
}

/// Start the panel when the control document is already loaded (Init path).
/// Startup failures are injected via script — safe here because signup HTML is
/// not on screen. After signup, callers must bake the error into `html(...)`
/// instead (see the signup IPC branch).
fn start_panel(
    supervisor: &Arc<Mutex<PanelSupervisor>>,
    webview: Option<&wry::WebView>,
    autostart: bool,
    hide_dock: bool,
    keep_awake: bool,
    update_version: Option<&str>,
    update_note: Option<&str>,
) {
    if let Err(e) = try_start_panel(supervisor) {
        // Keep the tray alive so the user can Quit / retry Resume.
        eprintln!("stitch-desktop: {e:#}");
        if let Some(wv) = webview {
            let script = control_ui::panel_error_script(&format!("{e:#}"));
            let _ = wv.evaluate_script(&script);
        }
    }
    sync_control_ui(
        webview,
        autostart,
        hide_dock,
        keep_awake,
        supervisor,
        update_version,
        update_note,
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

/// Apply a release poll without treating network failure as "up to date".
/// Returns the version to prompt about when a *manual* check found an update
/// (background polls stay quiet). `manual` comes from the poller kick flag —
/// not from `update_note`, so an in-flight background result can't steal the
/// kicked check's prompt.
fn apply_release_check(
    update_version: &mut Option<String>,
    update_asset: &mut Option<ReleaseAsset>,
    update_note: &mut Option<String>,
    result: ReleaseCheck,
    manual: bool,
) -> Option<String> {
    match result {
        ReleaseCheck::Available { latest, asset } => {
            let prompt = manual.then(|| latest.clone());
            *update_version = Some(latest);
            *update_asset = Some(asset);
            // Keep "Checking…" if a manual kick is still outstanding (this
            // result was a concurrent background poll). Manual results clear it.
            if manual || update_note.as_deref() != Some(CHECKING_UPDATES_NOTE) {
                *update_note = None;
            }
            prompt
        }
        ReleaseCheck::Current => {
            *update_version = None;
            *update_asset = None;
            // Only show "up to date" after an explicit check. Background polls
            // stay quiet when current, and must not clear a pending Checking note.
            if manual {
                *update_note = Some("You're up to date.".into());
            } else if update_note.as_deref() != Some(CHECKING_UPDATES_NOTE) {
                *update_note = None;
            }
            None
        }
        ReleaseCheck::Failed { reason } => {
            // Keep a previously discovered update; a flaky check must not hide it.
            // Only surface the error after the operator asked us to check.
            if manual {
                *update_note = Some(format!("Couldn't check for updates ({reason})."));
            }
            None
        }
    }
}

/// Tray / window update action: confirm an available update, or kick an
/// immediate release check when no update is known yet.
fn handle_update_action(
    update_kick_tx: &Sender<()>,
    update_version: &mut Option<String>,
    update_note: &mut Option<String>,
    webview: Option<&wry::WebView>,
    autostart: bool,
    hide_dock: bool,
    keep_awake: bool,
    supervisor: &Arc<Mutex<PanelSupervisor>>,
    status_item: &IconMenuItem,
    pause_item: &IconMenuItem,
    update_item: &IconMenuItem,
    menu_icons: &MenuIcons,
    window: Option<&Window>,
) {
    if update_version.is_some() {
        show_control_window(window);
        if let Some(wv) = webview {
            let _ = wv.evaluate_script(control_ui::show_update_dialog_script());
        }
        return;
    }
    *update_note = Some(CHECKING_UPDATES_NOTE.into());
    let _ = update_kick_tx.send(());
    sync_tray_and_window(
        status_item,
        pause_item,
        update_item,
        menu_icons,
        webview,
        autostart,
        hide_dock,
        keep_awake,
        supervisor,
        update_version.as_deref(),
        update_note.as_deref(),
    );
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
    keep_awake_item: &IconMenuItem,
    menu_icons: &MenuIcons,
    tray: Option<&TrayIcon>,
    update_kick_tx: &Sender<()>,
    update_version: &mut Option<String>,
    update_asset: Option<&ReleaseAsset>,
    update_note: &mut Option<String>,
    update_in_progress: &mut bool,
    download_proxy: &tao::event_loop::EventLoopProxy<UserEvent>,
    status_item: &IconMenuItem,
    pause_item: &IconMenuItem,
    update_item: &IconMenuItem,
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
            handle_update_action(
                update_kick_tx,
                update_version,
                update_note,
                webview,
                *autostart_enabled,
                prefs.hide_dock_icon,
                prefs.keep_awake,
                supervisor,
                status_item,
                pause_item,
                update_item,
                menu_icons,
                None,
            );
            return;
        }
        "download_update" => {
            start_update_download(
                update_version.as_deref(),
                update_asset,
                update_note,
                update_in_progress,
                download_proxy,
            );
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
            apply_keep_awake(
                enabled,
                keep_awake,
                prefs,
                paths,
                keep_awake_item,
                menu_icons,
                tray,
            );
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
        update_version.as_deref(),
        update_note.as_deref(),
    );
}

fn start_update_download(
    update_version: Option<&str>,
    update_asset: Option<&ReleaseAsset>,
    update_note: &mut Option<String>,
    update_in_progress: &mut bool,
    proxy: &tao::event_loop::EventLoopProxy<UserEvent>,
) {
    if *update_in_progress {
        return;
    }
    let (Some(version), Some(asset)) = (update_version, update_asset) else {
        *update_note = Some("Check for updates before downloading.".into());
        return;
    };

    *update_in_progress = true;
    *update_note = Some("Downloading and verifying the update…".into());
    let version = version.to_owned();
    let asset = asset.clone();
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        let result = stitch_bot::update::download_desktop_update_blocking(&version, &asset)
            .map_err(|error| format!("{error:#}"));
        let _ = proxy.send_event(UserEvent::UpdateDownloadResult(result));
    });
}

fn apply_keep_awake(
    enabled: bool,
    controller: &mut KeepAwakeController,
    prefs: &mut DesktopPrefs,
    paths: &paths::DesktopPaths,
    menu_item: &IconMenuItem,
    menu_icons: &MenuIcons,
    tray: Option<&TrayIcon>,
) {
    match controller.set_enabled(enabled) {
        Ok(()) => {
            prefs.keep_awake = enabled;
            if let Err(e) = prefs.save(paths) {
                eprintln!("saving desktop prefs failed: {e:#}");
            }
            menu_icons::apply_keep_awake(menu_item, enabled, menu_icons);
            apply_tray_keep_awake_chrome(tray, enabled);
        }
        Err(e) => {
            eprintln!("keep awake failed: {e:#}");
            prefs.keep_awake = false;
            let _ = controller.set_enabled(false);
            menu_icons::apply_keep_awake(menu_item, false, menu_icons);
            apply_tray_keep_awake_chrome(tray, false);
            if let Err(save_err) = prefs.save(paths) {
                eprintln!("saving desktop prefs failed: {save_err:#}");
            }
        }
    }
}

fn apply_tray_keep_awake_chrome(tray: Option<&TrayIcon>, keep_awake: bool) {
    let Some(tray) = tray else { return };
    let icon = tray_icon_for_state(keep_awake);
    // Normal is a system-tinted template. Awake contains a yellow dot and is
    // already tinted for the current appearance, so preserve its color.
    #[cfg(target_os = "macos")]
    let icon_result = tray.set_icon_with_as_template(Some(icon), !keep_awake);
    #[cfg(not(target_os = "macos"))]
    let icon_result = tray.set_icon(Some(icon));
    if let Err(e) = icon_result {
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
    update_note: Option<&str>,
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
        update_note,
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
        // `rundll32 url.dll,FileProtocolHandler` opens the default browser
        // without a visible console. `cmd /C start` flashes a black window.
        let mut cmd = std::process::Command::new("rundll32");
        cmd.args(["url.dll,FileProtocolHandler", url]);
        win_cmd::no_window(&mut cmd);
        cmd.status().context("open URL")?;
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

/// Original grandma tray icon plus a small Textile-yellow "on" dot.
///
/// This state is not a macOS template because template tinting would erase the
/// yellow. Tint the grandma ourselves for the current light/dark appearance.
fn tray_icon_with_awake_badge() -> Option<Icon> {
    const SIZE: u32 = 32;
    const RGBA: &[u8] = include_bytes!("../../../assets/grandma-tray-32.rgba");
    if RGBA.len() != (SIZE * SIZE * 4) as usize {
        return None;
    }
    let mut px = RGBA.to_vec();
    tint_opaque_pixels(&mut px, menu_icons::current_ink());
    paint_awake_dot(&mut px, SIZE);
    Icon::from_rgba(px, SIZE, SIZE).ok()
}

fn tint_opaque_pixels(rgba: &mut [u8], ink: (u8, u8, u8)) {
    for pixel in rgba.chunks_exact_mut(4) {
        if pixel[3] > 0 {
            pixel[0] = ink.0;
            pixel[1] = ink.1;
            pixel[2] = ink.2;
        }
    }
}

/// Paint a crisp status dot in the upper-right, with a transparent separation
/// ring so the original grandma remains legible at 1×.
fn paint_awake_dot(rgba: &mut [u8], size: u32) {
    const CX: f32 = 25.0;
    const CY: f32 = 7.0;
    const DOT_R: f32 = 3.4;
    const KNOCKOUT_R: f32 = 4.7;
    const YELLOW: (u8, u8, u8) = (0xf7, 0xcc, 0x1e);

    for y in 0..size as i32 {
        for x in 0..size as i32 {
            let dx = x as f32 + 0.5 - CX;
            let dy = y as f32 + 0.5 - CY;
            let distance = (dx * dx + dy * dy).sqrt();
            let i = ((y as u32 * size + x as u32) * 4) as usize;

            if distance <= KNOCKOUT_R + 0.5 {
                let keep = (distance - (KNOCKOUT_R - 0.5)).clamp(0.0, 1.0);
                rgba[i + 3] = (rgba[i + 3] as f32 * keep).round() as u8;
            }

            let dot_alpha = (DOT_R + 0.5 - distance).clamp(0.0, 1.0);
            if dot_alpha > 0.0 {
                rgba[i] = YELLOW.0;
                rgba[i + 1] = YELLOW.1;
                rgba[i + 2] = YELLOW.2;
                rgba[i + 3] = (dot_alpha * 255.0).round() as u8;
            }
        }
    }
}

fn fallback_icon() -> Icon {
    tray_icon_from_embedded().expect("grandma tray icon")
}

#[cfg(test)]
mod tray_badge_tests {
    use super::{paint_awake_dot, tint_opaque_pixels};

    #[test]
    fn awake_dot_is_small_yellow_and_antialiased() {
        let mut rgba = vec![0u8; 32 * 32 * 4];
        paint_awake_dot(&mut rgba, 32);
        let yellow: Vec<&[u8]> = rgba
            .chunks_exact(4)
            .filter(|p| p[0..3] == [0xf7, 0xcc, 0x1e] && p[3] > 0)
            .collect();
        assert!(
            (25..=60).contains(&yellow.len()),
            "yellow pixels: {}",
            yellow.len()
        );
        assert!(
            yellow.iter().any(|p| p[3] == 255),
            "dot needs a solid center"
        );
        assert!(
            yellow.iter().any(|p| p[3] > 0 && p[3] < 255),
            "dot edge should be anti-aliased"
        );
    }

    #[test]
    fn awake_grandma_tint_preserves_alpha() {
        let mut rgba = vec![0, 0, 0, 0, 0, 0, 0, 128, 0, 0, 0, 255];
        tint_opaque_pixels(&mut rgba, (242, 242, 247));
        assert_eq!(&rgba[0..4], &[0, 0, 0, 0]);
        assert_eq!(&rgba[4..8], &[242, 242, 247, 128]);
        assert_eq!(&rgba[8..12], &[242, 242, 247, 255]);
    }
}

#[cfg(test)]
mod pause_confirmation_tests {
    use super::{PAUSE_CONFIRMATION_MESSAGE, PAUSE_CONFIRMATION_TITLE};

    #[test]
    fn confirmation_explains_selective_bot_restore() {
        assert_eq!(PAUSE_CONFIRMATION_TITLE, "Pause Stitch?");
        assert!(PAUSE_CONFIRMATION_MESSAGE.contains("every bot that is running now"));
        assert!(PAUSE_CONFIRMATION_MESSAGE.contains("restarts only those bots"));
        assert!(PAUSE_CONFIRMATION_MESSAGE.contains("already paused stay paused"));
    }
}

#[cfg(test)]
mod update_available_prompt_tests {
    use super::{UPDATE_AVAILABLE_TITLE, UPDATE_LATER_BUTTON, UPDATE_UPGRADE_BUTTON};

    #[test]
    fn prompt_copy_matches_upgrade_later_flow() {
        assert_eq!(UPDATE_AVAILABLE_TITLE, "Update available");
        assert_eq!(UPDATE_UPGRADE_BUTTON, "Upgrade");
        assert_eq!(UPDATE_LATER_BUTTON, "Later");
    }
}

#[cfg(test)]
mod release_state_tests {
    use super::{apply_release_check, ReleaseAsset, ReleaseCheck, CHECKING_UPDATES_NOTE};

    fn asset() -> ReleaseAsset {
        ReleaseAsset {
            name: "Stitch.dmg".into(),
            browser_download_url: "https://example.com/Stitch.dmg".into(),
            digest: Some(format!("sha256:{}", "a".repeat(64))),
        }
    }

    #[test]
    fn failed_manual_poll_preserves_a_known_update() {
        let mut version = Some("0.2.0".into());
        let mut selected_asset = Some(asset());
        let mut note = Some(CHECKING_UPDATES_NOTE.into());
        let prompt = apply_release_check(
            &mut version,
            &mut selected_asset,
            &mut note,
            ReleaseCheck::Failed {
                reason: "offline".into(),
            },
            true,
        );
        assert_eq!(version.as_deref(), Some("0.2.0"));
        assert!(selected_asset.is_some());
        assert_eq!(
            note.as_deref(),
            Some("Couldn't check for updates (offline).")
        );
        assert!(prompt.is_none());
    }

    #[test]
    fn current_release_clears_a_previous_update() {
        let mut version = Some("0.2.0".into());
        let mut selected_asset = Some(asset());
        let mut note = None;
        let prompt = apply_release_check(
            &mut version,
            &mut selected_asset,
            &mut note,
            ReleaseCheck::Current,
            false,
        );
        assert!(version.is_none());
        assert!(selected_asset.is_none());
        assert!(note.is_none());
        assert!(prompt.is_none());
    }

    #[test]
    fn manual_available_check_returns_prompt_version() {
        let mut version = None;
        let mut selected_asset = None;
        let mut note = Some(CHECKING_UPDATES_NOTE.into());
        let prompt = apply_release_check(
            &mut version,
            &mut selected_asset,
            &mut note,
            ReleaseCheck::Available {
                latest: "0.3.0".into(),
                asset: asset(),
            },
            true,
        );
        assert_eq!(version.as_deref(), Some("0.3.0"));
        assert!(selected_asset.is_some());
        assert!(note.is_none());
        assert_eq!(prompt.as_deref(), Some("0.3.0"));
    }

    #[test]
    fn background_available_check_stays_quiet() {
        let mut version = None;
        let mut selected_asset = None;
        let mut note = None;
        let prompt = apply_release_check(
            &mut version,
            &mut selected_asset,
            &mut note,
            ReleaseCheck::Available {
                latest: "0.3.0".into(),
                asset: asset(),
            },
            false,
        );
        assert_eq!(version.as_deref(), Some("0.3.0"));
        assert!(prompt.is_none());
    }

    #[test]
    fn background_result_does_not_steal_pending_manual_prompt() {
        // Kick while a background poll is in flight: note is already Checking,
        // but the arriving result is still tagged background.
        let mut version = None;
        let mut selected_asset = None;
        let mut note = Some(CHECKING_UPDATES_NOTE.into());
        let prompt = apply_release_check(
            &mut version,
            &mut selected_asset,
            &mut note,
            ReleaseCheck::Available {
                latest: "0.3.0".into(),
                asset: asset(),
            },
            false,
        );
        assert_eq!(version.as_deref(), Some("0.3.0"));
        assert!(prompt.is_none());
        assert_eq!(note.as_deref(), Some(CHECKING_UPDATES_NOTE));
    }

    #[test]
    fn background_current_keeps_checking_note() {
        let mut version = Some("0.2.0".into());
        let mut selected_asset = Some(asset());
        let mut note = Some(CHECKING_UPDATES_NOTE.into());
        let prompt = apply_release_check(
            &mut version,
            &mut selected_asset,
            &mut note,
            ReleaseCheck::Current,
            false,
        );
        assert!(version.is_none());
        assert!(prompt.is_none());
        assert_eq!(note.as_deref(), Some(CHECKING_UPDATES_NOTE));
    }
}

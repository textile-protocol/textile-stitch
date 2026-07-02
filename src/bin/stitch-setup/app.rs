// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Shared GUI state: which view we're on, the supervised child process, and the
//! shared rolling log buffer the reader threads append to.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};

use stitch_bot::setup::{self, ConfigPaths, Corridor, SpreadEdit, Status};

/// Cap the in-memory log so a long run can't grow unbounded.
const MAX_LOG_LINES: usize = 5000;

pub enum View {
    Setup,
    Panel,
    Settings,
}

/// Editable state for the Settings screen. Populated from the current config when
/// the screen opens; `key_input` stays empty unless the operator chooses to swap
/// the wallet.
#[derive(Default)]
pub struct SettingsForm {
    pub rpc_url: String,
    pub feed_url: String,
    pub buy: SpreadEdit,
    pub sell: SpreadEdit,
    /// Pools in the config; the screen edits the first and notes when there's more.
    pub pool_count: usize,
    /// True once "Change wallet…" is clicked, revealing the private-key field.
    pub change_wallet: bool,
    pub key_input: String,
    /// True once "Switch corridor…" is clicked, revealing the corridor picker.
    pub switch_corridor: bool,
    /// Catalog index the operator has picked in the switch picker. Defaults to the
    /// currently-configured corridor when the screen opens.
    pub corridor_choice: usize,
    pub error: Option<String>,
}

pub struct StitchApp {
    pub view: View,
    pub dir: PathBuf,
    pub paths: ConfigPaths,
    pub stitch_bin: Option<PathBuf>,

    // Setup form state.
    pub selected_corridor: usize,
    pub key_input: String,
    pub setup_error: Option<String>,
    /// Set when Create was pressed on an already-configured folder; requires a
    /// second, explicit confirmation before overwriting an existing key/config.
    pub pending_overwrite: bool,

    // Panel state.
    pub corridor: Option<&'static Corridor>,
    pub operator: Option<String>,
    pub status: Status,
    pub dry_run: bool,
    pub action_note: Option<String>,
    pub logs: Arc<Mutex<VecDeque<String>>>,
    /// Textile mark shown in the header (loaded once at startup).
    pub icon: Option<egui::TextureHandle>,
    /// Newer release version if the background check found one. Shared with the
    /// worker thread that queries GitHub, hence the Arc/Mutex.
    pub available_update: Arc<Mutex<Option<String>>>,
    /// Guards the one-shot update check so it's spawned once, not every frame.
    update_check_started: bool,
    child: Option<Child>,

    /// Settings screen form state (loaded when the screen opens).
    pub settings: SettingsForm,
}

/// The GUI's default config folder: ~/Stitch (always user-writable, unlike the
/// app bundle the executable may live in). Matches the README's foreground
/// location.
fn default_gui_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Stitch")
}

impl StitchApp {
    pub fn new(icon: Option<egui::TextureHandle>) -> Self {
        let dir = default_gui_dir();
        let paths = setup::config_paths(&dir);
        let configured = setup::is_configured(&dir);
        let corridor = configured
            .then(|| std::fs::read_to_string(&paths.toml).ok())
            .flatten()
            .and_then(|t| setup::identify_corridor(&t));
        let operator = configured
            .then(|| setup::operator_address(&dir).ok())
            .flatten()
            .map(|a| format!("{a:?}"));
        Self {
            view: if configured { View::Panel } else { View::Setup },
            dir,
            paths,
            stitch_bin: setup::find_stitch_binary(),
            selected_corridor: 0,
            key_input: String::new(),
            setup_error: None,
            pending_overwrite: false,
            corridor,
            operator,
            status: Status::Stopped,
            dry_run: false,
            action_note: None,
            logs: Arc::new(Mutex::new(VecDeque::new())),
            icon,
            available_update: Arc::new(Mutex::new(None)),
            update_check_started: false,
            child: None,
            settings: SettingsForm::default(),
        }
    }

    /// Load the current config into the Settings form and switch to that screen.
    /// A folder shown in the panel is always configured, so a read failure just
    /// surfaces as an inline error on an otherwise-empty form.
    pub fn open_settings(&mut self) {
        let mut form = SettingsForm {
            corridor_choice: self.current_corridor_index(),
            ..SettingsForm::default()
        };
        match std::fs::read_to_string(&self.paths.toml)
            .map_err(|e| e.to_string())
            .and_then(|t| setup::read_settings(&t).map_err(|e| format!("{e:#}")))
        {
            Ok(v) => {
                form.rpc_url = v.rpc_url;
                form.feed_url = v.feed_url;
                form.buy = v.buy;
                form.sell = v.sell;
                form.pool_count = v.pool_count;
            }
            Err(e) => form.error = Some(format!("Couldn't read the current config: {e}")),
        }
        self.settings = form;
        self.view = View::Settings;
    }

    /// Leave the Settings screen without saving, wiping any pasted key first.
    pub fn close_settings(&mut self) {
        use zeroize::Zeroize;
        self.settings.key_input.zeroize();
        self.settings = SettingsForm::default();
        self.view = View::Panel;
    }

    /// The catalog index of the currently-configured corridor, or 0 (the default
    /// preset) when the config doesn't match a known corridor.
    pub fn current_corridor_index(&self) -> usize {
        self.corridor
            .and_then(|c| setup::catalog().iter().position(|x| x.id == c.id))
            .unwrap_or(0)
    }

    /// Replace stitch.toml with a different corridor preset, keeping the operator
    /// wallet. This is a whole-config swap — the new preset ships its own RPC,
    /// feed and spreads, so the current corridor's endpoint/spread edits are
    /// discarded. A running bot is stopped rather than restarted: the new
    /// corridor's tokens need approving on their chain first, so the operator is
    /// sent back to the panel to approve and start. On a write failure nothing
    /// changes and the error shows inline on the settings screen.
    pub fn switch_corridor(&mut self) {
        self.settings.error = None;
        let Some(target) = setup::catalog().get(self.settings.corridor_choice) else {
            self.settings.error = Some("That corridor is no longer available.".into());
            return;
        };
        // A no-op switch would pointlessly overwrite the config (and any unsaved
        // field edits) with the same preset; treat it as just closing the picker.
        if self.corridor.is_some_and(|c| c.id == target.id) {
            self.settings.switch_corridor = false;
            return;
        }
        if let Err(e) = setup::write_toml_atomic(&self.paths.toml, target.toml_template) {
            self.settings.error = Some(format!("Couldn't switch corridor: {e:#}"));
            return;
        }
        let was_running = self.status != Status::Stopped;
        if was_running {
            self.stop_bot();
        }
        self.corridor = Some(target);
        let where_to = format!("{} · {}", target.display_name, target.network_label);
        self.action_note = Some(if was_running {
            format!(
                "Switched to {where_to}. The bot was stopped — approve tokens for the new corridor, then Start."
            )
        } else {
            format!("Switched to {where_to}. Approve tokens for the new corridor before starting.")
        });
        self.close_settings();
    }

    /// Validate and persist the Settings form: rewrite stitch.toml (comments
    /// preserved, atomic), optionally swap the wallet key, then restart the bot if
    /// it was running so the new config takes effect. On any failure nothing is
    /// left half-applied and the error shows inline; the screen stays open.
    pub fn save_settings(&mut self, ctx: &egui::Context) {
        self.settings.error = None;
        let current = match std::fs::read_to_string(&self.paths.toml) {
            Ok(t) => t,
            Err(e) => {
                self.settings.error = Some(format!("Couldn't read the config: {e}"));
                return;
            }
        };
        let patch = setup::SettingsPatch {
            rpc_url: self.settings.rpc_url.clone(),
            feed_url: self.settings.feed_url.clone(),
            buy: self.settings.buy.clone(),
            sell: self.settings.sell.clone(),
        };
        let edited = match setup::apply_settings(&current, &patch) {
            Ok(s) => s,
            Err(e) => {
                self.settings.error = Some(format!("{e:#}"));
                return;
            }
        };

        // Wallet swap is opt-in: only when the field was revealed and filled.
        // Validate the key BEFORE touching disk so an invalid key aborts with
        // nothing written, rather than leaving the config saved but the key
        // unchanged (an all-or-nothing save).
        let new_key = self.settings.change_wallet && !self.settings.key_input.trim().is_empty();
        if new_key {
            if let Err(e) = stitch_bot::signer::parse_private_key(self.settings.key_input.trim()) {
                self.settings.error = Some(format!("The new wallet key is not valid: {e:#}"));
                return;
            }
        }

        if let Err(e) = setup::write_toml_atomic(&self.paths.toml, &edited) {
            self.settings.error = Some(format!("Couldn't save the config: {e:#}"));
            return;
        }

        if new_key {
            use zeroize::Zeroize;
            let result = setup::write_key(&self.dir, &self.settings.key_input);
            self.settings.key_input.zeroize();
            match result {
                Ok(addr) => {
                    self.operator = Some(format!("{addr:?}"));
                    self.settings.change_wallet = false;
                }
                Err(e) => {
                    // Pre-validated above, so this is an unexpected write failure
                    // (e.g. permissions); the config is already saved.
                    self.settings.error = Some(format!(
                        "Settings saved, but writing the new wallet key failed: {e:#}"
                    ));
                    return;
                }
            }
        }

        // Config is read once at start, so a running bot must be bounced to pick
        // up the change. dry_run is preserved across the restart.
        if self.status != Status::Stopped {
            self.stop_bot();
            self.start_bot(ctx);
            if self.status == Status::Stopped {
                // Respawn failed (e.g. the stitch binary went missing). start_bot
                // recorded why in action_note; keep it visible instead of a
                // misleading success, so the operator sees the bot is offline.
                let why = self
                    .action_note
                    .take()
                    .unwrap_or_else(|| "the bot failed to start".into());
                self.action_note =
                    Some(format!("Settings saved, but the bot didn't restart. {why}"));
            } else {
                self.action_note = Some("Settings saved. Bot restarted.".into());
            }
        } else {
            self.action_note = Some("Settings saved.".into());
        }
        self.close_settings();
    }

    /// Keyboard shortcuts: Cmd/Ctrl+, opens Settings from the panel (the standard
    /// Preferences chord), Esc backs out of Settings.
    pub fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let (open, back) = ctx.input(|i| {
            (
                i.modifiers.command && i.key_pressed(egui::Key::Comma),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if open && matches!(self.view, View::Panel) {
            self.open_settings();
        }
        if back && matches!(self.view, View::Settings) {
            self.close_settings();
        }
    }

    /// Kick off a one-shot, best-effort "is a newer release out?" check on a
    /// worker thread. Safe to call every frame: it only spawns once. On success
    /// it stores the version and requests a repaint so the nudge appears.
    pub fn check_for_update(&mut self, ctx: &egui::Context) {
        if self.update_check_started {
            return;
        }
        self.update_check_started = true;
        let slot = self.available_update.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Some(version) = stitch_bot::update::newer_release_blocking() {
                *slot.lock().unwrap() = Some(version);
                ctx.request_repaint();
            }
        });
    }

    pub fn push_log(logs: &Arc<Mutex<VecDeque<String>>>, line: String) {
        let mut buf = logs.lock().unwrap();
        if buf.len() >= MAX_LOG_LINES {
            buf.pop_front();
        }
        buf.push_back(line);
    }

    /// Reload panel metadata after a successful setup write.
    pub fn refresh_after_setup(&mut self) {
        self.paths = setup::config_paths(&self.dir);
        self.corridor = std::fs::read_to_string(&self.paths.toml)
            .ok()
            .and_then(|t| setup::identify_corridor(&t));
        self.operator = setup::operator_address(&self.dir)
            .ok()
            .map(|a| format!("{a:?}"));
        self.view = View::Panel;
    }

    /// Spawn the bot (honouring the dry-run toggle), streaming output to `logs`.
    pub fn start_bot(&mut self, ctx: &egui::Context) {
        if self.child.is_some() {
            return;
        }
        let Some(bin) = self.stitch_bin.clone() else {
            self.action_note = Some("Couldn't find the stitch binary next to this app.".into());
            return;
        };
        let mut cmd = setup::run_command(&bin, &self.paths, self.dry_run);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        match cmd.spawn() {
            Ok(mut child) => {
                self.spawn_readers(&mut child, ctx.clone());
                self.child = Some(child);
                self.status = if self.dry_run {
                    Status::DryRun
                } else {
                    Status::Running
                };
                self.action_note = None;
            }
            Err(e) => self.action_note = Some(format!("Failed to start: {e}")),
        }
    }

    fn spawn_readers(&self, child: &mut Child, ctx: egui::Context) {
        let log_path = self.paths.log.clone();
        for stream in [
            child.stdout.take().map(Reader::Out),
            child.stderr.take().map(Reader::Err),
        ]
        .into_iter()
        .flatten()
        {
            let logs = self.logs.clone();
            let ctx = ctx.clone();
            let log_path = log_path.clone();
            std::thread::spawn(move || {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .ok();
                let reader: Box<dyn BufRead> = match stream {
                    Reader::Out(s) => Box::new(BufReader::new(s)),
                    Reader::Err(s) => Box::new(BufReader::new(s)),
                };
                for line in reader.lines().map_while(Result::ok) {
                    // The bot disables ANSI on a pipe, but an older bot binary may
                    // still colorize; strip escape codes so the pane and log file
                    // show clean text instead of literal "\x1b[2m…" sequences.
                    let line = strip_ansi(&line);
                    if let Some(f) = file.as_mut() {
                        let _ = writeln!(f, "{line}");
                    }
                    StitchApp::push_log(&logs, line);
                    ctx.request_repaint();
                }
            });
        }
    }

    /// Gracefully stop the bot if it's running. Safe to call when stopped.
    pub fn stop_bot(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = setup::terminate(&mut child);
            let _ = child.wait();
        }
        self.status = Status::Stopped;
    }

    /// Reap the child if it exited on its own, so the status reflects reality.
    pub fn poll_child(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if matches!(child.try_wait(), Ok(Some(_))) {
                self.child = None;
                self.status = Status::Stopped;
            }
        }
    }

    /// Run a one-shot `stitch` subcommand (approve / update), streaming to logs.
    pub fn run_oneshot(&mut self, mut cmd: std::process::Command, ctx: &egui::Context) {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        match cmd.spawn() {
            Ok(mut child) => {
                self.spawn_readers(&mut child, ctx.clone());
                // Detach: a reaper thread waits so we don't block the UI.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => self.action_note = Some(format!("Command failed to start: {e}")),
        }
    }
}

enum Reader {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

/// Remove ANSI escape sequences (CSI `\x1b[…<letter>`, plus stray `\x1b`) so log
/// lines render as plain text. egui draws no terminal colors, so colorized output
/// would otherwise appear as literal `\x1b[2m…` noise.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // ESC: drop a CSI sequence (`[` then params, ended by a letter); a lone
        // ESC (or any other escape form) is just dropped.
        if chars.clone().next() == Some('[') {
            chars.next(); // consume '['
            for nc in chars.by_ref() {
                if nc.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

impl eframe::App for StitchApp {
    // eframe 0.35 hands the app a `Ui` for the root viewport rather than a bare
    // `Context`; the view modules attach their own panels with `show_inside`.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        crate::theme::apply(ui.ctx());
        self.poll_child();
        self.handle_shortcuts(ui.ctx());
        match self.view {
            View::Setup => crate::wizard::show(self, ui),
            View::Panel => crate::panel::show(self, ui),
            View::Settings => crate::settings::show(self, ui),
        }
    }

    fn on_exit(&mut self) {
        // Lifecycle A: closing the window stops the bot.
        self.stop_bot();
    }
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn strips_sgr_color_codes() {
        let raw = "\u{1b}[2m2026\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m stitch: starting";
        // The space after the timestamp plus the level's leading pad survive (as
        // they do in real tracing output); only the escape codes are removed.
        assert_eq!(strip_ansi(raw), "2026  INFO stitch: starting");
    }

    #[test]
    fn leaves_plain_text_untouched() {
        assert_eq!(
            strip_ansi("posted ask ladder orders=1"),
            "posted ask ladder orders=1"
        );
    }

    #[test]
    fn drops_a_lone_escape() {
        assert_eq!(strip_ansi("a\u{1b}b"), "ab");
    }
}

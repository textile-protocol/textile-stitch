// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Shared GUI state: which view we're on, the supervised child process, and the
//! shared rolling log buffer the reader threads append to.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use stitch_bot::setup::{self, ConfigPaths, Corridor, SignerView, SpreadEdit, Status};

use crate::signerform::SignerForm;

/// Cap the in-memory log pane so a long run can't grow the app's memory
/// unbounded.
const MAX_LOG_LINES: usize = 5000;

/// Cap the on-disk `stitch.log` to the same window as the pane. The heartbeat
/// keeps the log ticking, and it also accumulates across every start, so
/// without this the file would grow without bound.
const MAX_LOG_FILE_LINES: usize = 5000;

/// Re-trim the log file this often (in lines written) during a long single run,
/// so it stays bounded between restarts too. Trimming keeps the last
/// `MAX_LOG_FILE_LINES`, so the ceiling is roughly that plus this interval.
const LOG_TRIM_EVERY_LINES: usize = 1000;

/// Never read more than this much of the log when trimming. The trim runs on the
/// GUI start path, and an operator upgrading may already have a multi-GB log (the
/// unbounded growth this bound exists to fix) — reading it whole would OOM/freeze
/// the app. 8 MiB comfortably holds `MAX_LOG_FILE_LINES` normal lines, so we only
/// ever scan the tail. Keep it well above `MAX_LOG_FILE_LINES × typical line`.
const LOG_TAIL_SCAN_BYTES: u64 = 8 * 1024 * 1024;

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
    /// Whether the taker leg is on: fill users' resting limit orders when they
    /// cross the bot's own quote.
    pub taker_enabled: bool,
    /// Pools in the config; the screen edits the first and notes when there's more.
    pub pool_count: usize,
    /// True once "Change signer…" is clicked, revealing the signer editor.
    pub change_signer: bool,
    /// Signer editor state, prefilled from the current config when the screen opens.
    pub signer: SignerForm,
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
    pub signer_form: SignerForm,
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

    /// macOS install location, resolved once at startup. `None` off macOS or when
    /// not running from a `.app` bundle. Drives the "move to Applications" nudge.
    pub mac_install: Option<setup::macos::MacInstall>,
    /// Set once the operator dismisses the move nudge, so it stays hidden for the
    /// session.
    pub install_nudge_dismissed: bool,
    /// Inline error from a failed move attempt, shown in the nudge.
    pub install_error: Option<String>,
}

/// The operator address to show for a config. The configured `[signer]` wins: an
/// MPC signer carries its own `operator_address`, so reading it first means a
/// folder switched from hot wallet to MPC (or an MPC folder opened at startup)
/// shows the MPC address even if a stale stitch.key is still on disk. Only the hot
/// wallet derives the address from the key file.
fn operator_for(paths: &ConfigPaths) -> Option<String> {
    if let Ok(toml) = std::fs::read_to_string(&paths.toml) {
        match setup::read_signer(&toml) {
            SignerView::Turnkey {
                operator_address, ..
            }
            | SignerView::Mpcvault {
                operator_address, ..
            } => return Some(operator_address),
            SignerView::Local => {}
        }
    }
    setup::operator_address(&paths.dir)
        .ok()
        .map(|a| format!("{a:?}"))
}

/// The GUI's default config folder: ~/Stitch (always user-writable, unlike the
/// app bundle the executable may live in). Matches the README's foreground
/// location. Only used when the operator hasn't set up into a custom folder — a
/// remembered folder wins (see [`StitchApp::new`]).
fn default_gui_dir() -> PathBuf {
    setup::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Stitch")
}

/// Where to open on launch: the folder the operator set up into last time if it
/// still holds a config, otherwise the default ~/Stitch. Without the remembered
/// folder, setting up into any custom directory (via Browse) would send the
/// operator back through the wizard on every restart, since startup would only
/// ever check the default.
fn startup_dir() -> PathBuf {
    // A remembered folder from a prior setup always wins.
    if let Some(dir) = setup::remembered_config_dir().filter(|d| setup::is_configured(d)) {
        return dir;
    }
    // Prefer the current documented default (~/Stitch, USERPROFILE-based on
    // Windows) whenever it already holds a config, so a fresh setup there always
    // wins over a stale legacy folder — checking legacy first could reopen an old
    // operator/corridor after the user has already set up the default.
    let default = default_gui_dir();
    if setup::is_configured(&default) {
        return default;
    }
    // Only when the default is empty do we fall back to locations older builds
    // used. Pre-pointer builds resolved ~ via HOME first, so a Windows operator
    // who launched from Git Bash/MSYS can have a valid config under HOME/Stitch.
    // Adopt the first legacy location that's actually configured and remember it,
    // so the migration costs exactly one launch instead of re-running the wizard.
    if let Some(legacy) = setup::legacy_gui_dirs()
        .into_iter()
        .find(|d| setup::is_configured(d))
    {
        setup::remember_config_dir(&legacy);
        return legacy;
    }
    default
}

impl StitchApp {
    pub fn new(icon: Option<egui::TextureHandle>) -> Self {
        let dir = startup_dir();
        let paths = setup::config_paths(&dir);
        let configured = setup::is_configured(&dir);
        let corridor = configured
            .then(|| std::fs::read_to_string(&paths.toml).ok())
            .flatten()
            .and_then(|t| setup::identify_corridor(&t));
        let operator = configured.then(|| operator_for(&paths)).flatten();
        Self {
            view: if configured { View::Panel } else { View::Setup },
            dir,
            paths,
            stitch_bin: setup::find_stitch_binary(),
            selected_corridor: 0,
            signer_form: SignerForm::default(),
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
            mac_install: setup::macos::detect(),
            install_nudge_dismissed: false,
            install_error: None,
        }
    }

    /// Copy the app into /Applications and relaunch from there, then close this
    /// (translocated / out-of-place) instance. Stops the bot first so nothing is
    /// left supervising from the old location. On failure the app stays put and
    /// the reason shows inline in the nudge so the operator can drag it by hand.
    pub fn install_to_applications(&mut self, ctx: &egui::Context) {
        self.install_error = None;
        let Some(install) = self.mac_install.clone() else {
            return;
        };
        self.stop_bot();
        match install.install() {
            Ok(target) => {
                setup::macos::open(&target);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(e) => {
                self.install_error = Some(format!(
                    "{e:#} Drag Stitch into Applications from the download instead."
                ));
            }
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
        match std::fs::read_to_string(&self.paths.toml) {
            Ok(toml) => {
                match setup::read_settings(&toml) {
                    Ok(v) => {
                        form.rpc_url = v.rpc_url;
                        form.feed_url = v.feed_url;
                        form.buy = v.buy;
                        form.sell = v.sell;
                        form.taker_enabled = v.taker_enabled;
                        form.pool_count = v.pool_count;
                    }
                    Err(e) => form.error = Some(format!("Couldn't read the current config: {e:#}")),
                }
                // Prefill the signer editor from the current config (secrets stay
                // blank; the operator re-enters one only when changing signer).
                form.signer = SignerForm::from_view(&setup::read_signer(&toml));
            }
            Err(e) => form.error = Some(format!("Couldn't read the current config: {e}")),
        }
        self.settings = form;
        self.view = View::Settings;
    }

    /// Leave the Settings screen without saving, wiping any pasted secrets first.
    pub fn close_settings(&mut self) {
        self.settings.signer.zeroize_secrets();
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
        if let Err(e) = setup::switch_corridor_preserving_signer(&self.dir, target.toml_template) {
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
            taker_enabled: self.settings.taker_enabled,
        };
        let edited = match setup::apply_settings(&current, &patch) {
            Ok(s) => s,
            Err(e) => {
                self.settings.error = Some(format!("{e:#}"));
                return;
            }
        };

        // The signer (wallet vs MPC) is changed on its own, via apply_signer_change,
        // because it rewrites the secret + env too; this save is endpoints/spreads.
        if let Err(e) = setup::write_toml_atomic(&self.paths.toml, &edited) {
            self.settings.error = Some(format!("Couldn't save the config: {e:#}"));
            return;
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
        // Remember where this config landed so the next launch reopens straight
        // into the panel — even when the operator set up into a custom folder
        // rather than the default ~/Stitch.
        setup::remember_config_dir(&self.dir);
        self.paths = setup::config_paths(&self.dir);
        self.corridor = std::fs::read_to_string(&self.paths.toml)
            .ok()
            .and_then(|t| setup::identify_corridor(&t));
        self.operator = self.read_operator();
        self.view = View::Panel;
    }

    /// Adopt the folder the operator browsed to when it already holds a complete
    /// config, instead of overwriting it. This is the upgrade path for someone who
    /// set up into a custom folder before the location pointer existed: startup
    /// can't rediscover an arbitrary old path, so they Browse to it — and without
    /// this they'd have to run Create, which replaces their stitch.toml and signer
    /// secret. Here we just remember the folder and load the panel from what's
    /// already on disk; nothing is written. Any secret typed into the form is wiped
    /// since it's unused.
    pub fn adopt_existing(&mut self) {
        self.signer_form.zeroize_secrets();
        self.setup_error = None;
        self.pending_overwrite = false;
        // refresh_after_setup persists the pointer and loads panel metadata from
        // the existing files — exactly what adopting needs, minus any write.
        self.refresh_after_setup();
    }

    /// The operator address to show for the current config (see [`operator_for`]).
    fn read_operator(&self) -> Option<String> {
        operator_for(&self.paths)
    }

    /// Apply a signer change from the Settings screen: rewrite the `[signer]`
    /// section, the secret file, and stitch.env, then bounce the bot if running.
    /// Leaves everything else untouched. On failure nothing is applied and the
    /// error shows inline.
    pub fn apply_signer_change(&mut self, ctx: &egui::Context) {
        self.settings.error = None;
        let setup = self.settings.signer.to_setup();
        if let Err(e) = setup::apply_signer(&self.dir, &setup) {
            self.settings.error = Some(format!("{e:#}"));
            return;
        }
        self.settings.signer.zeroize_secrets();
        self.operator = self.read_operator();

        if self.status != Status::Stopped {
            self.stop_bot();
            self.start_bot(ctx);
            if self.status == Status::Stopped {
                // start_bot recorded why it failed in action_note; keep that
                // instead of a misleading "restarted" so the operator sees the bot
                // is offline after switching custody.
                let why = self
                    .action_note
                    .take()
                    .unwrap_or_else(|| "the bot failed to start".into());
                self.action_note =
                    Some(format!("Signer updated, but the bot didn't restart. {why}"));
            } else {
                self.action_note = Some("Signer updated. Bot restarted.".into());
            }
        } else {
            self.action_note = Some("Signer updated.".into());
        }
        self.close_settings();
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
        // Bound the file up front: it accumulates across every start, so a fresh
        // run inherits whatever the last runs left behind.
        trim_log_file(&log_path, MAX_LOG_FILE_LINES);

        // One writer owns the file so trimming (truncate + rewrite tail) can't
        // race the append from the other stream. Both reader threads feed it.
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || log_writer(&log_path, rx));

        for stream in [
            child.stdout.take().map(Reader::Out),
            child.stderr.take().map(Reader::Err),
        ]
        .into_iter()
        .flatten()
        {
            let logs = self.logs.clone();
            let ctx = ctx.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let reader: Box<dyn BufRead> = match stream {
                    Reader::Out(s) => Box::new(BufReader::new(s)),
                    Reader::Err(s) => Box::new(BufReader::new(s)),
                };
                for line in reader.lines().map_while(Result::ok) {
                    // The bot disables ANSI on a pipe, but an older bot binary may
                    // still colorize; strip escape codes so the pane and log file
                    // show clean text instead of literal "\x1b[2m…" sequences.
                    let line = strip_ansi(&line);
                    let _ = tx.send(line.clone());
                    StitchApp::push_log(&logs, line);
                    ctx.request_repaint();
                }
            });
        }
        // Drop the original sender so the writer stops once both readers finish.
        drop(tx);
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

/// Sole owner of the log file: append each received line, and every
/// `LOG_TRIM_EVERY_LINES` re-trim to the last `MAX_LOG_FILE_LINES` so a long run
/// stays bounded. Runs until every sender (the reader threads) has dropped.
fn log_writer(log_path: &Path, rx: mpsc::Receiver<String>) {
    let open = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .ok()
    };
    let mut file = open();
    let mut since_trim = 0usize;
    for line in rx {
        if let Some(f) = file.as_mut() {
            let _ = writeln!(f, "{line}");
        }
        since_trim += 1;
        if since_trim >= LOG_TRIM_EVERY_LINES {
            since_trim = 0;
            // Close before rewriting so the truncate isn't fighting our own
            // append handle, then reopen at the new (trimmed) end.
            drop(file.take());
            trim_log_file(log_path, MAX_LOG_FILE_LINES);
            file = open();
        }
    }
}

/// Rewrite `path` in place keeping only its last `keep` lines. Reads at most the
/// last `LOG_TAIL_SCAN_BYTES`, so a huge log is bounded in memory rather than
/// materialized whole. Best-effort: an unreadable file is left untouched.
fn trim_log_file(path: &Path, keep: usize) {
    let Some((tail, whole)) = read_tail(path, LOG_TAIL_SCAN_BYTES) else {
        return;
    };
    // If we saw the whole file and it's within the cap, leave it untouched.
    // When we only scanned the tail, the file is far past the cap by definition,
    // so always rewrite it down to the last `keep` lines.
    if whole && tail.bytes().filter(|&b| b == b'\n').count() <= keep {
        return;
    }
    let _ = std::fs::write(path, tail_lines(&tail, keep));
}

/// Read at most the last `max_bytes` of `path`. Returns the bytes as a string
/// (lossy, since a tail window can split a UTF-8 char at its start) and whether
/// that was the whole file. `None` if the file can't be opened/read.
fn read_tail(path: &Path, max_bytes: u64) -> Option<(String, bool)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let whole = len <= max_bytes;
    if !whole {
        f.seek(SeekFrom::Start(len - max_bytes)).ok()?;
    }
    let mut buf = Vec::with_capacity(len.min(max_bytes) as usize);
    f.take(max_bytes).read_to_end(&mut buf).ok()?;
    Some((String::from_utf8_lossy(&buf).into_owned(), whole))
}

/// The last `keep` lines of `contents`, newline-terminated. Pure, so the trim
/// policy is unit-testable without touching the filesystem.
fn tail_lines(contents: &str, keep: usize) -> String {
    if keep == 0 {
        return String::new();
    }
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(keep);
    let mut out: String = lines[start..].join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
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
        // Sits above whichever view renders below, so a misplaced app is flagged
        // on both the setup wizard and the control panel.
        crate::install::show_nudge(self, ui);
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
    use super::{read_tail, strip_ansi, tail_lines, trim_log_file};

    fn temp_log(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("stitch-log-{}-{name}", std::process::id()))
    }

    #[test]
    fn read_tail_windows_a_large_file_from_the_end() {
        let path = temp_log("readtail");
        let content = "aaaa\nbbbb\ncccc\ndddd\n"; // 20 bytes
        std::fs::write(&path, content).unwrap();
        // Cap below the file size: only the last 10 bytes come back, not whole.
        let (tail, whole) = read_tail(&path, 10).unwrap();
        assert!(!whole);
        assert_eq!(tail, "cccc\ndddd\n");
        // A cap above the file size returns everything and reports whole.
        let (all, whole) = read_tail(&path, 1024).unwrap();
        assert!(whole);
        assert_eq!(all, content);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn trim_log_file_keeps_the_tail_and_leaves_a_short_log_alone() {
        let path = temp_log("trim");
        let big: String = (0..50).map(|i| format!("line{i}\n")).collect();
        std::fs::write(&path, &big).unwrap();
        trim_log_file(&path, 10);
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after.lines().count(), 10);
        assert!(after.starts_with("line40\n"));
        assert!(after.ends_with("line49\n"));
        // Re-trimming a file already within the cap is a no-op.
        trim_log_file(&path, 10);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tail_keeps_only_the_last_lines_newline_terminated() {
        let input = "a\nb\nc\nd\ne\n";
        assert_eq!(tail_lines(input, 2), "d\ne\n");
    }

    #[test]
    fn tail_leaves_a_short_log_intact() {
        // Fewer lines than the cap: every line survives.
        assert_eq!(tail_lines("a\nb\n", 5), "a\nb\n");
    }

    #[test]
    fn tail_of_zero_is_empty() {
        assert_eq!(tail_lines("a\nb\nc\n", 0), "");
    }

    #[test]
    fn tail_handles_a_missing_final_newline() {
        assert_eq!(tail_lines("a\nb\nc", 2), "b\nc\n");
    }

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

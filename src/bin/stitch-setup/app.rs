// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Shared GUI state: which view we're on, the supervised child process, and the
//! shared rolling log buffer the reader threads append to.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};

use stitch_bot::setup::{self, ConfigPaths, Corridor, Status};

/// Cap the in-memory log so a long run can't grow unbounded.
const MAX_LOG_LINES: usize = 5000;

pub enum View {
    Setup,
    Panel,
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
    child: Option<Child>,
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
    pub fn new() -> Self {
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
            child: None,
        }
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

impl eframe::App for StitchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_child();
        match self.view {
            View::Setup => crate::wizard::show(self, ctx),
            View::Panel => crate::panel::show(self, ctx),
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Lifecycle A: closing the window stops the bot.
        self.stop_bot();
    }
}

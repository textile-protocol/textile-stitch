// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! What bots exist on this host, and what's wrong with them.
//!
//! The panel keeps no database. The fleet is derived every time from two sources
//! of truth that already exist: the container list from the daemon, and the
//! config directories on disk. That means an operator can always fix things
//! behind the panel's back — edit a TOML, `docker rm` a container — and the next
//! page load tells the truth.
//!
//! Adoption is the reason this module is more than a filter. A hand-written
//! compose fleet carries no Stitch labels, so a bot is recognised from its image
//! and its mount table, and its config directory is recovered from where the
//! daemon says `stitch.toml` came from. Nothing is restarted to adopt it: because
//! the config is bind-mounted rather than baked into the image, the panel can
//! edit an adopted bot's config in place and restart it, which is exactly what an
//! operator does by hand today.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::panel::config::PanelConfig;
use crate::panel::docker::{ContainerInfo, ContainerState, MountInfo};
use crate::panel::naming::{
    id_from_container_name, LABEL_BOT, LABEL_COMPOSE_PROJECT, LABEL_COMPOSE_SERVICE, LABEL_ONE_SHOT,
};
use crate::setup;

/// Where the bot expects its runtime directory inside the container. Fixed by the
/// image's `CMD` (`--config /home/stitch/run/stitch.toml`).
pub const RUN_DIR: &str = "/home/stitch/run";

/// Where the bot expects its config inside the container.
pub const RUN_TOML: &str = "/home/stitch/run/stitch.toml";

/// How a bot's files are mounted, which decides whether its slot-nonce ledger
/// survives the container being recreated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// The config directory is mounted read-write at the run dir, with the config
    /// and key re-mounted read-only on top. The ledger lands on the host and
    /// survives recreation. This is what the panel creates.
    Directory,
    /// Only the individual files are mounted, so the run directory is inside the
    /// container. The bot writes its slot-nonce ledger next to the config, which
    /// means into the container filesystem — lost on every recreation, after
    /// which the bot mints fresh nonces and cannot replace its still-live orders
    /// until they expire.
    FlatFiles,
    /// No recognisable config mount. The config may be baked into a custom image
    /// or supplied through `STITCH_CONFIG_TOML`, which the panel can't edit.
    Unknown,
}

impl Layout {
    /// Whether the slot-nonce ledger reaches the host.
    pub fn persists_ledger(self) -> bool {
        matches!(self, Layout::Directory)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Layout::Directory => "directory",
            Layout::FlatFiles => "flat-files",
            Layout::Unknown => "unknown",
        }
    }
}

/// Who created this bot, which decides how much the panel can safely change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Created by the panel; carries our bot label.
    Panel,
    /// Created by `docker compose`. Fully controllable, but compose can still
    /// act on it independently.
    Compose {
        project: Option<String>,
        service: String,
    },
    /// A Stitch image started some other way (plain `docker run`, another tool).
    Foreign,
    /// A config directory with no container at all — a bot that was created and
    /// then had its container removed, or one half-created by a failed wizard run.
    ConfigOnly,
}

impl Origin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Origin::Panel => "panel",
            Origin::Compose { .. } => "compose",
            Origin::Foreign => "foreign",
            Origin::ConfigOnly => "config-only",
        }
    }
}

/// Something an operator should know about a bot. These are surfaced in the UI
/// rather than fixed silently, because every one of them has a trade-off only the
/// operator can weigh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// The flat-file layout loses the slot-nonce ledger on recreation.
    LedgerNotPersisted,
    /// The config lives outside the directory tree mounted into the panel, so the
    /// panel can see the bot but cannot read or edit its config.
    ConfigOutsideBotsRoot { host_path: PathBuf },
    /// The config path is known but couldn't be read.
    ConfigUnreadable { detail: String },
    /// The config was read but doesn't parse as a valid bot config. The bot will
    /// fail to start with the same error.
    ConfigInvalid { detail: String },
    /// No config mount found, so there's nothing for the panel to edit.
    ConfigNotMounted,
    /// Compose also manages this container, and `docker compose up -d` can undo
    /// what the panel does.
    ComposeManaged { project: Option<String> },
    /// The config has more than one pool; the settings form edits one at a time.
    MultiPool { pools: usize },
    /// Two containers resolve to the same bot name, so actions would be
    /// ambiguous.
    DuplicateName { containers: Vec<String> },
}

impl Warning {
    /// A short operator-facing sentence. Kept next to the variant so the API and
    /// the UI can't drift out of sync on what a warning means.
    pub fn message(&self) -> String {
        match self {
            Warning::LedgerNotPersisted => format!(
                "Only the config files are mounted, so {RUN_DIR} lives inside the container. \
                 The slot-nonce ledger is lost whenever the container is recreated, and the \
                 bot then can't replace its still-live orders until they expire. \
                 Migrate to the per-bot directory layout to fix this."
            ),
            Warning::ConfigOutsideBotsRoot { host_path } => format!(
                "This bot's config is at {} on the host, outside the directory mounted into \
                 the panel. You can start and stop it, but not edit its settings.",
                host_path.display()
            ),
            Warning::ConfigUnreadable { detail } => {
                format!("Couldn't read this bot's config: {detail}")
            }
            Warning::ConfigInvalid { detail } => format!(
                "This bot's config is not valid, and the bot will fail to start with the \
                 same error: {detail}"
            ),
            Warning::ConfigNotMounted => format!(
                "No config file is mounted at {RUN_TOML}. The config is probably baked into \
                 a custom image or passed through STITCH_CONFIG_TOML, so the panel can't \
                 edit it."
            ),
            Warning::ComposeManaged { project } => {
                let which = project
                    .as_deref()
                    .map(|p| format!(" \"{p}\""))
                    .unwrap_or_default();
                format!(
                    "Docker Compose project{which} also manages this bot. Running \
                     `docker compose up -d` can restart a bot you paused here. Export a \
                     compose file from the panel to keep the two in agreement."
                )
            }
            Warning::MultiPool { pools } => format!(
                "This bot quotes {pools} corridors on this chain. Pick which one to edit in \
                 Settings."
            ),
            Warning::DuplicateName { containers } => format!(
                "More than one container claims this bot name ({}). Rename or remove one \
                 before using the panel's controls on it.",
                containers.join(", ")
            ),
        }
    }

    /// A stable machine-readable tag, so the UI can style or act on a specific
    /// warning without string-matching the prose in [`Self::message`].
    pub fn kind(&self) -> &'static str {
        match self {
            Warning::LedgerNotPersisted => "ledgerNotPersisted",
            Warning::ConfigOutsideBotsRoot { .. } => "configOutsideBotsRoot",
            Warning::ConfigUnreadable { .. } => "configUnreadable",
            Warning::ConfigInvalid { .. } => "configInvalid",
            Warning::ConfigNotMounted => "configNotMounted",
            Warning::ComposeManaged { .. } => "composeManaged",
            Warning::MultiPool { .. } => "multiPool",
            Warning::DuplicateName { .. } => "duplicateName",
        }
    }

    /// Whether this blocks editing rather than just warning about it.
    pub fn blocks_editing(&self) -> bool {
        matches!(
            self,
            Warning::ConfigOutsideBotsRoot { .. }
                | Warning::ConfigNotMounted
                | Warning::DuplicateName { .. }
        )
    }

    /// Whether this blocks starting, stopping or removing the bot too.
    ///
    /// Narrower than [`Self::blocks_editing`], and for a different reason. A bot
    /// whose config the panel can't reach is still one container that the panel
    /// can safely start and stop. A duplicate name isn't: the fleet collapses the
    /// containers into one entry, so an action would hit whichever one discovery
    /// happened to pick, and removing with the config would delete files out from
    /// under a container that's still trading.
    pub fn blocks_actions(&self) -> bool {
        matches!(self, Warning::DuplicateName { .. })
    }
}

/// The non-secret parts of a bot's config, for the fleet list and the settings
/// form. Parsed through the real loader so what's shown is what the bot would
/// load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSummary {
    /// Catalog corridor id, when the config matches a shipped corridor.
    pub corridor_id: Option<String>,
    /// Human label, e.g. "cNGN / USDT on BNB Smart Chain".
    pub corridor_label: Option<String>,
    pub chain_id: u64,
    pub pools: usize,
    /// The signing address. Derived from the key file for a hot wallet, read from
    /// `[signer]` for MPC. Never the key itself.
    pub operator_address: Option<String>,
    /// Which signer backend the config selects.
    pub signer: String,
    /// Whether a bot on this config broadcasts transactions from the operator
    /// wallet, rather than only signing orders offchain.
    ///
    /// The maker leg signs Permit2 orders and posts them to the book, which costs
    /// no nonce. The taker (`limit_taker_enabled`) and closer legs call the reactor
    /// on chain, and each transaction consumes an account nonce. That's what makes
    /// a second process holding the same key unsafe to run concurrently, so the
    /// panel needs to know before it offers one.
    pub sends_transactions: bool,
}

/// One bot in the fleet.
#[derive(Debug, Clone)]
pub struct Bot {
    /// Routing key: the bot name used in URLs and as the config directory name.
    pub name: String,
    pub origin: Origin,
    pub layout: Layout,
    /// Container name, absent for a config directory with no container.
    pub container_name: Option<String>,
    pub state: ContainerState,
    /// The daemon's human-readable status, e.g. "Up 3 hours".
    pub status: String,
    pub image: Option<String>,
    /// Content-addressed image id (`sha256:…`) of the running container, when
    /// known. Update detection keys off this rather than a mutable tag, so
    /// pulling `:latest` for one bot doesn't make its siblings look current.
    pub image_id: Option<String>,
    pub created_unix: Option<i64>,
    /// Host path of `stitch.toml`, as the daemon reports it.
    pub config_host_path: Option<PathBuf>,
    /// Path the panel can read `stitch.toml` at, when it's reachable.
    pub config_panel_path: Option<PathBuf>,
    pub config: Option<ConfigSummary>,
    pub warnings: Vec<Warning>,
}

impl Bot {
    /// Whether the panel can edit this bot's config.
    pub fn is_editable(&self) -> bool {
        self.config_panel_path.is_some() && !self.warnings.iter().any(Warning::blocks_editing)
    }

    /// The container to act on. An error rather than an `Option` because every
    /// lifecycle action needs one and "this bot has no container" is a real,
    /// reportable state.
    pub fn require_container(&self) -> Result<&str> {
        self.container_name.as_deref().ok_or_else(|| {
            anyhow!(
                "{} has a config directory but no container. Recreate it from the panel to \
                 start it.",
                self.name
            )
        })
    }

    /// The config directory the panel reads and writes.
    pub fn config_dir(&self) -> Option<PathBuf> {
        self.config_panel_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf))
    }

    /// The signing identity this bot transacts as, when the panel can work it out.
    ///
    /// Two bots with the same pair draw from one account nonce sequence, however
    /// separate their containers and config directories look — the same key in two
    /// directories is an ordinary way to run two corridors on one chain. Anything
    /// that must not collide with a live bot's transactions has to compare this,
    /// not the bot name.
    ///
    /// `None` when there's no readable config or no operator address yet (a
    /// directory whose key hasn't been written). Nothing can be signed in that
    /// state anyway, so there is no nonce to collide over.
    pub fn wallet(&self) -> Option<WalletId> {
        let config = self.config.as_ref()?;
        let address = config.operator_address.as_ref()?;
        Some(WalletId {
            chain_id: config.chain_id,
            // Hot wallets get a checksummed address derived from the key file; an
            // MPC config carries whatever the operator typed. Compare lowercase so
            // two spellings of one address aren't mistaken for two wallets.
            address: address.to_lowercase(),
        })
    }
}

/// A chain and an address on it: the scope an account nonce sequence lives in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WalletId {
    pub chain_id: u64,
    /// Lowercased, so casing can't split one wallet into two.
    pub address: String,
}

impl std::fmt::Display for WalletId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} on chain {}", self.address, self.chain_id)
    }
}

/// Every bot on the host, keyed by name.
#[derive(Debug, Clone, Default)]
pub struct Fleet {
    bots: Vec<Bot>,
}

impl Fleet {
    pub fn bots(&self) -> &[Bot] {
        &self.bots
    }

    pub fn len(&self) -> usize {
        self.bots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bots.is_empty()
    }

    /// Look a bot up by name. Errors rather than returning `None` so handlers get
    /// a message they can return verbatim.
    pub fn get(&self, name: &str) -> Result<&Bot> {
        self.bots
            .iter()
            .find(|b| b.name == name)
            .ok_or_else(|| anyhow!("there is no bot called \"{name}\""))
    }

    /// Whether a name is already taken, by a container or by a config directory.
    /// Checked by the wizard before writing anything.
    pub fn contains(&self, name: &str) -> bool {
        self.bots.iter().any(|b| b.name == name)
    }
}

/// Build the fleet from the container list plus whatever is on disk.
///
/// Containers win over bare directories: a directory with a container is one bot,
/// not two. Directories with no container are still listed, so a bot whose
/// container was removed doesn't vanish along with the operator's ability to see
/// its config.
pub fn discover(containers: &[ContainerInfo], cfg: &PanelConfig) -> Fleet {
    // BTreeMap so the fleet list is stable across page loads regardless of the
    // order the daemon happened to return containers in.
    let mut by_name: BTreeMap<String, Bot> = BTreeMap::new();
    let mut duplicates: HashMap<String, Vec<String>> = HashMap::new();

    for c in containers.iter().filter(|c| is_stitch_container(c)) {
        let bot = bot_from_container(c, cfg);
        match by_name.entry(bot.name.clone()) {
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(bot);
            }
            std::collections::btree_map::Entry::Occupied(mut e) => {
                // Two containers claiming one name. Prefer the panel-native one
                // (it's the one whose layout we control) and record both so the
                // operator is told rather than silently losing one.
                let names = duplicates.entry(bot.name.clone()).or_default();
                if names.is_empty() {
                    if let Some(existing) = e.get().container_name.clone() {
                        names.push(existing);
                    }
                }
                if let Some(n) = bot.container_name.clone() {
                    names.push(n);
                }
                if bot.origin == Origin::Panel && e.get().origin != Origin::Panel {
                    e.insert(bot);
                }
            }
        }
    }

    for (name, containers) in duplicates {
        if let Some(bot) = by_name.get_mut(&name) {
            bot.warnings.push(Warning::DuplicateName { containers });
        }
    }

    for name in config_only_dirs(cfg, &by_name) {
        by_name.insert(name.clone(), bot_from_config_dir(&name, cfg));
    }

    Fleet {
        bots: by_name.into_values().collect(),
    }
}

/// Config directories under the bots root that no container claims.
fn config_only_dirs(cfg: &PanelConfig, known: &BTreeMap<String, Bot>) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(&cfg.bots_dir) else {
        // No bots root yet (fresh install) is normal, not an error.
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        // Only directories that actually hold a config, so a stray folder in the
        // bots root doesn't show up as a phantom bot.
        .filter(|name| cfg.bot_dir(name).join("stitch.toml").exists())
        .filter(|name| !known.contains_key(name))
        .collect()
}

/// Decide whether a container is a Stitch bot.
///
/// Three independent signals, because an adopted fleet carries none of our
/// labels: our own bot label, an image that looks like the Stitch image, or a
/// mount into the run directory the image expects its config at.
pub fn is_stitch_container(c: &ContainerInfo) -> bool {
    // A running `approve` or `dry-run` matches every one of those signals — same
    // image, same mounts — but it is a job against a bot, not a bot. Left in, it
    // shows up as a phantom second row with lifecycle buttons on it.
    if c.labels.contains_key(LABEL_ONE_SHOT) {
        return false;
    }
    // The panel itself, whatever it happens to be called. This has to come before
    // every positive signal below, including the bot label.
    if administers_docker(c) {
        return false;
    }
    c.labels.contains_key(LABEL_BOT)
        || image_looks_like_stitch(&c.image)
        || c.mounts.iter().any(|m| m.destination.starts_with(RUN_DIR))
}

/// Whether this container drives the Docker daemon, which makes it the panel rather
/// than a bot.
///
/// Keyed on the socket mount because that is a property of what the container *is*,
/// not of what it was named. A bot has no use for the Docker socket — it talks to an
/// RPC endpoint and a price feed. The panel cannot work without it.
///
/// [`PANEL_IMAGE_NAMES`] alone was not enough, and the way it failed is worth
/// recording. The documented install builds the image rather than pulling it, and
/// Compose tags a built image `<project>-<service>` — `stitch-bot-panel` for a
/// checkout of this repo. That is not the published name, so the denylist missed it,
/// and it matches the `stitch-` prefix, so the panel adopted itself as a bot called
/// `panel` and offered a Stop button that kills the process serving the page. The
/// name depends on the operator's directory, so no denylist can cover it.
fn administers_docker(c: &ContainerInfo) -> bool {
    c.mounts.iter().any(|m| {
        [&m.destination, &m.source]
            .into_iter()
            .any(|p| p.file_name().and_then(|n| n.to_str()) == Some("docker.sock"))
    })
}

/// Whether an image reference looks like the Stitch bot image. Matches the
/// published image, and locally built images named `stitch` or `stitch-*`, while
/// rejecting unrelated names that merely contain the word.
fn image_looks_like_stitch(image: &str) -> bool {
    // Strip a digest and then a tag, being careful that a registry port
    // (`host:5000/img`) is not mistaken for a tag separator.
    let no_digest = image.split('@').next().unwrap_or(image);
    let repo = match no_digest.rsplit_once(':') {
        Some((before, after)) if !after.contains('/') => before,
        _ => no_digest,
    };
    let last = repo.rsplit('/').next().unwrap_or(repo).to_lowercase();
    // The panel's own image name sits inside the `stitch-*` prefix, and the panel
    // must never adopt itself: it runs beside the bots on the same daemon, so an
    // operator clicking Stop on that row would kill the process serving the page.
    if PANEL_IMAGE_NAMES.contains(&last.as_str()) {
        return false;
    }
    last == "stitch"
        || last == "textile-stitch"
        || last.starts_with("stitch-")
        || last.starts_with("textile-stitch-")
}

/// Image names that are the panel itself rather than a bot.
const PANEL_IMAGE_NAMES: &[&str] = &["stitch-panel", "textile-stitch-panel"];

fn bot_from_container(c: &ContainerInfo, cfg: &PanelConfig) -> Bot {
    let origin = origin_of(c);
    let name = bot_name(c, &origin);
    let layout = layout_of(&c.mounts);
    let config_host_path = config_host_path(&c.mounts);

    let mut warnings = Vec::new();
    if layout == Layout::FlatFiles {
        warnings.push(Warning::LedgerNotPersisted);
    }
    if let Origin::Compose { project, .. } = &origin {
        warnings.push(Warning::ComposeManaged {
            project: project.clone(),
        });
    }

    // Resolve the config to something the panel can actually open. A path outside
    // the mounted bots root comes back unchanged from `to_panel_path`, which is
    // how we detect that the bot is visible but not editable.
    let config_panel_path = match &config_host_path {
        None => {
            warnings.push(Warning::ConfigNotMounted);
            None
        }
        Some(host) => {
            let panel_path = cfg.to_panel_path(host);
            if panel_path.exists() {
                Some(panel_path)
            } else if let Some(own) = panel_native_config(&origin, &name, cfg) {
                Some(own)
            } else {
                warnings.push(Warning::ConfigOutsideBotsRoot {
                    host_path: host.clone(),
                });
                None
            }
        }
    };

    let config = config_panel_path
        .as_ref()
        .and_then(|p| read_summary(p, &mut warnings));

    if let Some(summary) = &config {
        if summary.pools > 1 {
            warnings.push(Warning::MultiPool {
                pools: summary.pools,
            });
        }
    }

    Bot {
        name,
        origin,
        layout,
        container_name: Some(c.name.clone()),
        state: c.state,
        status: c.status.clone(),
        image: Some(c.image.clone()),
        image_id: (!c.image_id.is_empty()).then(|| c.image_id.clone()),
        created_unix: Some(c.created_unix),
        config_host_path,
        config_panel_path,
        config,
        warnings,
    }
}

/// Where the panel put a bot's config, for a container carrying the panel's own
/// label.
///
/// The daemon doesn't always report a bind source the way it was passed in:
/// Docker Desktop prefixes `/host_mnt`, and a symlinked root (`/srv/stitch` ->
/// `/mnt/data/stitch`) comes back resolved. Deriving editability purely from the
/// reported path then makes the panel refuse to edit bots it created minutes ago.
/// Our own label is a stronger claim than the daemon's rendering of a path, so
/// trust it — but only for our own containers, since an adopted bot that happens
/// to share a name with a directory here would otherwise be pointed at the wrong
/// config.
fn panel_native_config(origin: &Origin, name: &str, cfg: &PanelConfig) -> Option<PathBuf> {
    if *origin != Origin::Panel {
        return None;
    }
    let path = cfg.bot_dir(name).join("stitch.toml");
    path.exists().then_some(path)
}

fn bot_from_config_dir(name: &str, cfg: &PanelConfig) -> Bot {
    let panel_path = cfg.bot_dir(name).join("stitch.toml");
    let mut warnings = Vec::new();
    let config = read_summary(&panel_path, &mut warnings);
    if let Some(summary) = &config {
        if summary.pools > 1 {
            warnings.push(Warning::MultiPool {
                pools: summary.pools,
            });
        }
    }
    Bot {
        name: name.to_string(),
        origin: Origin::ConfigOnly,
        // The panel writes this layout, so a directory it created is one even
        // before a container exists.
        layout: Layout::Directory,
        container_name: None,
        state: ContainerState::Unknown,
        status: "no container".to_string(),
        image: None,
        image_id: None,
        created_unix: None,
        config_host_path: Some(cfg.host_bot_dir(name).join("stitch.toml")),
        config_panel_path: Some(panel_path),
        config,
        warnings,
    }
}

fn origin_of(c: &ContainerInfo) -> Origin {
    if c.labels.contains_key(LABEL_BOT) {
        return Origin::Panel;
    }
    match c.label(LABEL_COMPOSE_SERVICE) {
        Some(service) => Origin::Compose {
            project: c.label(LABEL_COMPOSE_PROJECT).map(str::to_string),
            service: service.to_string(),
        },
        None => Origin::Foreign,
    }
}

/// The bot's routing name. Prefer our own label, then compose's service name (so
/// an adopted `bot-a` keeps the name the operator already uses), then the
/// container name with our prefix stripped.
fn bot_name(c: &ContainerInfo, origin: &Origin) -> String {
    if let Some(id) = c.label(LABEL_BOT) {
        return id.to_string();
    }
    if let Origin::Compose { service, .. } = origin {
        return service.clone();
    }
    id_from_container_name(&c.name)
        .unwrap_or(&c.name)
        .to_string()
}

/// Which layout a container's mounts represent. The deciding question is whether
/// anything writable lands on the host at the run directory, because that's where
/// the bot writes its slot-nonce ledger.
pub fn layout_of(mounts: &[MountInfo]) -> Layout {
    let run_dir = Path::new(RUN_DIR);
    if mounts.iter().any(|m| m.destination == run_dir && m.rw) {
        return Layout::Directory;
    }
    if config_host_path(mounts).is_some() {
        return Layout::FlatFiles;
    }
    Layout::Unknown
}

/// Host path of `stitch.toml`, from an explicit file mount if there is one, else
/// derived from a directory mount at the run dir.
pub fn config_host_path(mounts: &[MountInfo]) -> Option<PathBuf> {
    let toml = Path::new(RUN_TOML);
    if let Some(m) = mounts.iter().find(|m| m.destination == toml) {
        return Some(m.source.clone());
    }
    mounts
        .iter()
        .find(|m| m.destination == Path::new(RUN_DIR))
        .map(|m| m.source.join("stitch.toml"))
}

/// Read and summarise a config, recording why it failed rather than dropping it.
fn read_summary(path: &Path, warnings: &mut Vec<Warning>) -> Option<ConfigSummary> {
    // Worth naming before the read fails with a bare "Is a directory (os error 21)",
    // because the cause is not obvious and the fix is a one-liner. Docker creates a
    // bind mount's source as a directory when it doesn't exist, so a config that has
    // turned into a folder means a container was started against a path that wasn't
    // there — a wrong host bots dir, or a compose file naming a config nobody wrote.
    if path.is_dir() {
        warnings.push(Warning::ConfigUnreadable {
            detail: format!(
                "{} is a directory, not a file. Docker leaves one behind when a container is \
                 created with a bind mount whose source is missing, so this config was probably \
                 never written where the daemon looked for it. Remove the empty directory and \
                 restore the file.",
                path.display()
            ),
        });
        return None;
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            warnings.push(Warning::ConfigUnreadable {
                detail: e.to_string(),
            });
            return None;
        }
    };
    match summarise(&text, path) {
        Ok(s) => Some(s),
        Err(e) => {
            warnings.push(Warning::ConfigInvalid {
                detail: e.to_string(),
            });
            None
        }
    }
}

/// Summarise a config body. Parsing goes through the bot's own loader, so a
/// config the panel shows as valid is one the bot can start with.
///
/// `config_path` is the toml itself, not its parent: a flat-layout bot's key is
/// named after the config (`stitch.bot1.key`), and looking in the parent for a
/// canonical `stitch.key` would either miss it or pick up an unrelated bot's.
pub fn summarise(toml_str: &str, config_path: &Path) -> Result<ConfigSummary> {
    let parsed = crate::config::Config::from_toml(toml_str)?;
    let corridor = setup::identify_corridor(toml_str);
    let signer = setup::read_signer(toml_str);
    Ok(ConfigSummary {
        corridor_id: corridor.map(|c| c.id.to_string()),
        corridor_label: corridor.map(|c| format!("{} on {}", c.display_name, c.network_label)),
        chain_id: parsed.chain_id,
        pools: parsed.pools.len(),
        operator_address: operator_address(&signer, config_path),
        signer: signer_label(&signer).to_string(),
        sends_transactions: parsed
            .pools
            .iter()
            .any(|p| p.limit_taker_enabled() || p.closer_enabled()),
    })
}

/// The bot's signing address. MPC configs state it outright; a hot wallet's is
/// derived from the key file beside the config, which is why this needs the
/// config path. Returns `None` rather than failing the whole summary when the
/// key is missing — a config with no key yet is a real intermediate state.
fn operator_address(signer: &setup::SignerView, config_path: &Path) -> Option<String> {
    match signer {
        setup::SignerView::Local => {
            let key = crate::panel::provision::find_beside(config_path, "stitch.key")?;
            setup::operator_address_from_key(&key)
                .ok()
                .map(|a| format!("{a:?}"))
        }
        setup::SignerView::Turnkey {
            operator_address, ..
        }
        | setup::SignerView::Mpcvault {
            operator_address, ..
        } => Some(operator_address.clone()),
    }
}

fn signer_label(signer: &setup::SignerView) -> &'static str {
    match signer {
        setup::SignerView::Local => "hot-wallet",
        setup::SignerView::Turnkey { .. } => "turnkey",
        setup::SignerView::Mpcvault { .. } => "mpcvault",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::docker::fake::{container, dir_layout_mounts, flat_layout_mounts};
    use crate::panel::naming::LABEL_BOT;

    /// A panel config rooted at a real temp directory, so config reads exercise
    /// the same filesystem path the panel uses in production.
    fn test_cfg(tag: &str) -> (PanelConfig, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "stitch-panel-inv-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        // The host root is deliberately different from the panel view, so any
        // place that forgets to translate shows up as a failing test.
        let cfg = PanelConfig::for_test(root.clone(), "/host/stitch");
        (cfg, root)
    }

    /// Write a real corridor config into a bot directory.
    fn seed_bot_dir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(
            &dir,
            corridor,
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        dir
    }

    #[test]
    fn image_matching_accepts_the_real_image_and_rejects_lookalikes() {
        for image in [
            "ghcr.io/textile-protocol/textile-stitch:latest",
            "ghcr.io/textile-protocol/textile-stitch:sha-abc123",
            "textile-stitch",
            "stitch",
            "stitch:dev",
            "stitch-bot:local",
            "localhost:5000/textile-stitch:latest",
            "ghcr.io/textile-protocol/textile-stitch@sha256:abc",
        ] {
            assert!(
                image_looks_like_stitch(image),
                "{image} should be recognised"
            );
        }
        for image in ["postgres:16", "nginx", "ghcr.io/other/stitching-service:1"] {
            assert!(
                !image_looks_like_stitch(image),
                "{image} must not be recognised"
            );
        }
    }

    #[test]
    fn the_panel_never_adopts_itself() {
        // The panel runs on the same daemon as the bots it manages, and its image
        // name falls inside the `stitch-*` prefix. Listing it as a bot would put a
        // Stop button on the process serving the page.
        for image in [
            "ghcr.io/textile-protocol/textile-stitch-panel:latest",
            "textile-stitch-panel",
            "stitch-panel:local",
            "stitch-panel",
        ] {
            assert!(
                !image_looks_like_stitch(image),
                "{image} is the panel, not a bot"
            );
        }
    }

    #[test]
    fn a_running_one_shot_is_not_a_bot() {
        // approve and dry-run containers share the bot's image and mounts, so every
        // discovery signal fires on them. While one runs, the fleet must still show
        // one bot, not two — a phantom row would carry Stop and Delete buttons that
        // act on a job.
        let (cfg, root) = test_cfg("one-shot");
        seed_bot_dir(&root, "bot-a");

        let mut bot = container("stitch-bot-a", ContainerState::Running);
        bot.labels.insert(LABEL_BOT.into(), "bot-a".into());
        bot.mounts = dir_layout_mounts(&root.join("bot-a").display().to_string());

        let mut job = container("stitch-dryrun-bot-a-2d216fb0", ContainerState::Running);
        job.labels
            .insert(LABEL_ONE_SHOT.into(), "bot-a:dry-run".into());
        job.mounts = bot.mounts.clone();

        assert!(!is_stitch_container(&job));
        let fleet = discover(&[bot, job], &cfg);
        assert_eq!(fleet.len(), 1, "{:?}", fleet.bots());
        assert_eq!(fleet.bots()[0].name, "bot-a");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_locally_built_panel_image_is_not_a_bot() {
        // The documented install is `docker compose -f docker-compose.panel.yml up -d
        // --build` from this directory, and Compose tags a service's built image
        // `<project>-<service>` — so a checkout of this repo produces
        // `stitch-bot-panel`, not the published `textile-stitch-panel`. That slips
        // past the image denylist while still matching the `stitch-` prefix, so the
        // panel adopted *itself* as a bot called `panel` — with a Stop button that
        // kills the process serving the page.
        for image in [
            "stitch-bot-panel",
            "stitch-bot-panel:latest",
            "stitch-monorepo-panel",
        ] {
            let mut panel = container("stitch-panel", ContainerState::Running);
            panel.image = image.into();
            panel
                .labels
                .insert(LABEL_COMPOSE_SERVICE.into(), "panel".into());
            panel.mounts = panel_mounts();
            assert!(
                !is_stitch_container(&panel),
                "{image} is the panel, not a bot"
            );
        }
    }

    /// The mounts the shipped panel service has: the Docker socket, and the bots
    /// root. The socket is the signal — no bot has any use for it.
    fn panel_mounts() -> Vec<MountInfo> {
        vec![
            MountInfo {
                source: PathBuf::from("/var/run/docker.sock"),
                destination: PathBuf::from("/var/run/docker.sock"),
                rw: true,
            },
            MountInfo {
                source: PathBuf::from("/srv/stitch/bots"),
                destination: PathBuf::from("/data/bots"),
                rw: true,
            },
        ]
    }

    #[test]
    fn a_compose_managed_panel_container_is_not_in_the_fleet() {
        // End to end through discovery, with the compose labels a real
        // docker-compose.panel.yml deployment produces.
        let (cfg, root) = test_cfg("self");
        let mut panel = container("stitch-panel", ContainerState::Running);
        panel.image = "ghcr.io/textile-protocol/textile-stitch-panel:latest".into();
        panel
            .labels
            .insert(LABEL_COMPOSE_SERVICE.into(), "panel".into());
        panel.mounts = vec![
            MountInfo {
                source: PathBuf::from("/var/run/docker.sock"),
                destination: PathBuf::from("/var/run/docker.sock"),
                rw: true,
            },
            MountInfo {
                source: PathBuf::from("/srv/stitch/bots"),
                destination: PathBuf::from("/data/bots"),
                rw: true,
            },
        ];
        assert!(!is_stitch_container(&panel));
        assert!(discover(&[panel], &cfg).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_registry_port_is_not_mistaken_for_a_tag() {
        // "localhost:5000/textile-stitch" has a colon before the repo, not a tag.
        assert!(image_looks_like_stitch("localhost:5000/textile-stitch"));
    }

    #[test]
    fn the_directory_layout_persists_the_ledger_and_the_flat_one_does_not() {
        assert_eq!(
            layout_of(&dir_layout_mounts("/host/stitch/bot-a")),
            Layout::Directory
        );
        assert!(Layout::Directory.persists_ledger());

        let flat = flat_layout_mounts("/host/stitch", "bot1");
        assert_eq!(layout_of(&flat), Layout::FlatFiles);
        assert!(
            !Layout::FlatFiles.persists_ledger(),
            "the flat layout loses the nonce ledger on recreation"
        );
    }

    #[test]
    fn a_read_only_run_dir_mount_is_not_the_directory_layout() {
        // A read-only dir mount looks structurally right but the bot still can't
        // write its ledger, so it must not be reported as the good layout.
        let mounts = vec![MountInfo {
            source: PathBuf::from("/host/stitch/bot-a"),
            destination: PathBuf::from(RUN_DIR),
            rw: false,
        }];
        assert_ne!(layout_of(&mounts), Layout::Directory);
    }

    #[test]
    fn no_config_mount_at_all_is_unknown() {
        assert_eq!(layout_of(&[]), Layout::Unknown);
    }

    #[test]
    fn config_path_comes_from_the_file_mount_or_the_directory_mount() {
        let from_file = config_host_path(&flat_layout_mounts("/host/stitch", "bot1"));
        assert_eq!(
            from_file,
            Some(PathBuf::from("/host/stitch/stitch.bot1.toml")),
            "a renamed config file must be found by its container destination"
        );

        // Only a directory mount: the config is the conventional name inside it.
        let dir_only = vec![MountInfo {
            source: PathBuf::from("/host/stitch/bot-a"),
            destination: PathBuf::from(RUN_DIR),
            rw: true,
        }];
        assert_eq!(
            config_host_path(&dir_only),
            Some(PathBuf::from("/host/stitch/bot-a/stitch.toml"))
        );
    }

    #[test]
    fn a_panel_created_bot_is_recognised_by_its_label() {
        let (cfg, root) = test_cfg("panel-native");
        seed_bot_dir(&root, "bot-a");
        let mut c = container("stitch-bot-a", ContainerState::Running);
        c.labels.insert(LABEL_BOT.into(), "bot-a".into());
        c.mounts = dir_layout_mounts("/host/stitch/bot-a");

        let fleet = discover(&[c], &cfg);
        let bot = fleet.get("bot-a").unwrap();
        assert_eq!(bot.origin, Origin::Panel);
        assert_eq!(bot.layout, Layout::Directory);
        assert!(bot.state.is_running());
        assert!(bot.is_editable());
        assert!(bot.warnings.is_empty(), "got {:?}", bot.warnings);
        // The config was read through the real loader.
        let summary = bot.config.as_ref().unwrap();
        assert_eq!(summary.chain_id, 56);
        assert_eq!(summary.corridor_id.as_deref(), Some("cngn-usdt-bsc"));
        assert_eq!(summary.signer, "hot-wallet");
        assert_eq!(
            summary.operator_address.as_deref().map(str::to_lowercase),
            Some("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string())
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_compose_bot_is_adopted_under_its_service_name() {
        // The whole point of adoption: no Stitch labels, but recognisable from the
        // image, and it keeps the name the operator already types.
        let (cfg, root) = test_cfg("adopt");
        seed_bot_dir(&root, "bot-b");
        let mut c = container("stitch-bot-b", ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.into(), "bot-b".into());
        c.labels
            .insert(LABEL_COMPOSE_PROJECT.into(), "stitch".into());
        c.mounts = dir_layout_mounts("/host/stitch/bot-b");

        let fleet = discover(&[c], &cfg);
        let bot = fleet.get("bot-b").unwrap();
        assert_eq!(
            bot.origin,
            Origin::Compose {
                project: Some("stitch".into()),
                service: "bot-b".into()
            }
        );
        // Adopted bots are editable — that's what makes migration a non-event.
        assert!(bot.is_editable());
        assert!(bot
            .warnings
            .iter()
            .any(|w| matches!(w, Warning::ComposeManaged { .. })));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_flat_layout_bot_is_flagged_for_losing_its_ledger() {
        let (cfg, root) = test_cfg("flat");
        // The example compose layout keeps configs flat in one directory.
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        std::fs::write(root.join("stitch.bot1.toml"), corridor.toml_template).unwrap();
        // Its key, named after the config — not the canonical stitch.key.
        std::fs::write(
            root.join("stitch.bot1.key"),
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80\n",
        )
        .unwrap();
        // And an unrelated canonical key for another bot, which must not win.
        std::fs::write(
            root.join("stitch.key"),
            "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d\n",
        )
        .unwrap();

        let mut c = container("stitch-bot1", ContainerState::Running);
        c.labels.insert(LABEL_COMPOSE_SERVICE.into(), "bot1".into());
        c.mounts = flat_layout_mounts("/host/stitch", "bot1");

        let fleet = discover(&[c], &cfg);
        let bot = fleet.get("bot1").unwrap();
        assert_eq!(bot.layout, Layout::FlatFiles);
        assert!(bot.warnings.contains(&Warning::LedgerNotPersisted));
        // The warning explains the consequence, not just the fact.
        let msg = Warning::LedgerNotPersisted.message();
        assert!(msg.contains("slot-nonce ledger"));
        assert!(msg.contains("still-live orders"));
        // Address comes from stitch.bot1.key, not the unrelated stitch.key.
        assert_eq!(
            bot.config
                .as_ref()
                .and_then(|c| c.operator_address.as_deref())
                .map(str::to_lowercase),
            Some("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string())
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_config_directory_is_explained_rather_than_reported_as_errno_21() {
        // What the operator actually sees on the fleet page when Docker has replaced
        // their config with a folder. "Is a directory (os error 21)" is true and
        // useless; the cause and the fix are both one sentence.
        let (cfg, root) = test_cfg("configdir");
        std::fs::create_dir_all(root.join("bot-a").join("stitch.toml")).unwrap();
        let mut c = container("stitch-bot-a", ContainerState::Running);
        c.labels.insert(LABEL_BOT.into(), "bot-a".into());
        c.mounts = dir_layout_mounts(root.join("bot-a").to_str().unwrap());

        let bot = discover(&[c], &cfg).get("bot-a").cloned().unwrap();
        let detail = bot
            .warnings
            .iter()
            .find_map(|w| match w {
                Warning::ConfigUnreadable { detail } => Some(detail.clone()),
                _ => None,
            })
            .expect("the unreadable config has to be reported");
        assert!(detail.contains("is a directory, not a file"), "{detail}");
        assert!(
            detail.contains("bind mount whose source is missing"),
            "{detail}"
        );
        assert!(
            !detail.contains("os error"),
            "the raw errno is not the message: {detail}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_config_outside_the_bots_root_is_visible_but_not_editable() {
        let (cfg, root) = test_cfg("outside");
        let mut c = container("stitch-bot-x", ContainerState::Running);
        c.labels.insert(LABEL_BOT.into(), "bot-x".into());
        c.mounts = dir_layout_mounts("/somewhere/else/bot-x");

        let fleet = discover(&[c], &cfg);
        let bot = fleet.get("bot-x").unwrap();
        // Lifecycle still works, editing does not, and the operator is told why.
        assert!(bot.require_container().is_ok());
        assert!(!bot.is_editable());
        assert!(bot
            .warnings
            .iter()
            .any(|w| matches!(w, Warning::ConfigOutsideBotsRoot { .. })));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_rewritten_mount_source_does_not_lock_the_panel_out_of_its_own_bot() {
        // Docker Desktop reports the bind source as /host_mnt/<path>, so the
        // reported path doesn't sit under the configured host root. A bot the
        // panel created must stay editable through that.
        let (cfg, root) = test_cfg("rewritten");
        seed_bot_dir(&root, "bot-a");
        let mut c = container("stitch-bot-a", ContainerState::Running);
        c.labels.insert(LABEL_BOT.into(), "bot-a".into());
        c.mounts = dir_layout_mounts("/host_mnt/host/stitch/bot-a");

        let bot = discover(&[c], &cfg).get("bot-a").cloned().unwrap();
        assert!(bot.is_editable(), "warnings: {:?}", bot.warnings);
        assert_eq!(bot.config_panel_path, Some(root.join("bot-a/stitch.toml")));
        assert!(!bot
            .warnings
            .iter()
            .any(|w| matches!(w, Warning::ConfigOutsideBotsRoot { .. })));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_adopted_bot_is_not_pointed_at_a_same_named_directory() {
        // The name match is a coincidence, not a claim: only the panel's own label
        // licenses reading a config the mount table doesn't point at. Guessing here
        // would show one bot's settings while editing another's file.
        let (cfg, root) = test_cfg("coincidence");
        seed_bot_dir(&root, "bot-a");
        let mut c = container("bot-a", ContainerState::Running);
        c.labels
            .insert(LABEL_COMPOSE_SERVICE.into(), "bot-a".into());
        c.mounts = dir_layout_mounts("/somewhere/else/bot-a");

        let bot = discover(&[c], &cfg).get("bot-a").cloned().unwrap();
        assert!(!bot.is_editable());
        assert!(bot
            .warnings
            .iter()
            .any(|w| matches!(w, Warning::ConfigOutsideBotsRoot { .. })));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_invalid_config_is_reported_instead_of_hiding_the_bot() {
        let (cfg, root) = test_cfg("invalid");
        let dir = root.join("bot-bad");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stitch.toml"), "this is not toml {{{").unwrap();

        let mut c = container("stitch-bot-bad", ContainerState::Exited);
        c.labels.insert(LABEL_BOT.into(), "bot-bad".into());
        c.mounts = dir_layout_mounts("/host/stitch/bot-bad");

        let fleet = discover(&[c], &cfg);
        let bot = fleet.get("bot-bad").unwrap();
        assert!(bot.config.is_none());
        assert!(bot
            .warnings
            .iter()
            .any(|w| matches!(w, Warning::ConfigInvalid { .. })));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_config_directory_with_no_container_still_appears() {
        // Otherwise removing a container would hide the config and the operator
        // would have no way to see or recreate the bot.
        let (cfg, root) = test_cfg("orphan");
        seed_bot_dir(&root, "bot-orphan");
        let fleet = discover(&[], &cfg);
        let bot = fleet.get("bot-orphan").unwrap();
        assert_eq!(bot.origin, Origin::ConfigOnly);
        assert!(bot.container_name.is_none());
        assert!(!bot.state.is_running());
        // Acting on it names the actual problem.
        let err = bot.require_container().unwrap_err().to_string();
        assert!(err.contains("no container"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_container_and_its_directory_are_one_bot_not_two() {
        let (cfg, root) = test_cfg("nodupe");
        seed_bot_dir(&root, "bot-a");
        let mut c = container("stitch-bot-a", ContainerState::Running);
        c.labels.insert(LABEL_BOT.into(), "bot-a".into());
        c.mounts = dir_layout_mounts("/host/stitch/bot-a");

        let fleet = discover(&[c], &cfg);
        assert_eq!(fleet.len(), 1);
        assert!(fleet.get("bot-a").unwrap().container_name.is_some());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn two_containers_claiming_one_name_are_flagged_and_block_editing() {
        let (cfg, root) = test_cfg("dupe");
        seed_bot_dir(&root, "bot-a");

        let mut panel_bot = container("stitch-bot-a", ContainerState::Running);
        panel_bot.labels.insert(LABEL_BOT.into(), "bot-a".into());
        panel_bot.mounts = dir_layout_mounts("/host/stitch/bot-a");

        let mut compose_bot = container("other-bot-a", ContainerState::Running);
        compose_bot
            .labels
            .insert(LABEL_COMPOSE_SERVICE.into(), "bot-a".into());
        compose_bot.mounts = dir_layout_mounts("/host/stitch/bot-a");

        let fleet = discover(&[compose_bot, panel_bot], &cfg);
        assert_eq!(fleet.len(), 1);
        let bot = fleet.get("bot-a").unwrap();
        // The panel-native container wins, because its layout is the one we control.
        assert_eq!(bot.origin, Origin::Panel);
        let dup = bot
            .warnings
            .iter()
            .find(|w| matches!(w, Warning::DuplicateName { .. }))
            .expect("duplicate must be reported");
        assert!(dup.blocks_editing());
        assert!(!bot.is_editable());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn non_stitch_containers_are_ignored() {
        let (cfg, root) = test_cfg("ignore");
        let mut pg = container("postgres", ContainerState::Running);
        pg.image = "postgres:16".into();
        pg.mounts = vec![MountInfo {
            source: PathBuf::from("/var/lib/pg"),
            destination: PathBuf::from("/var/lib/postgresql/data"),
            rw: true,
        }];
        assert!(!is_stitch_container(&pg));
        assert!(discover(&[pg], &cfg).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_stray_directory_without_a_config_is_not_a_bot() {
        let (cfg, root) = test_cfg("stray");
        std::fs::create_dir_all(root.join("notes")).unwrap();
        assert!(discover(&[], &cfg).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_fleet_lists_bots_alphabetically_by_name() {
        let (cfg, root) = test_cfg("alpha");
        seed_bot_dir(&root, "zeta");
        seed_bot_dir(&root, "alpha");
        seed_bot_dir(&root, "mid");
        let fleet = discover(&[], &cfg);
        let names: Vec<_> = fleet.bots().iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_missing_bots_root_yields_an_empty_fleet_rather_than_an_error() {
        let cfg = PanelConfig::for_test("/definitely/not/here", "/definitely/not/here");
        assert!(discover(&[], &cfg).is_empty());
    }

    #[test]
    fn looking_up_an_unknown_bot_names_it_in_the_error() {
        let fleet = Fleet::default();
        let err = fleet.get("ghost").unwrap_err().to_string();
        assert!(err.contains("ghost"));
        assert!(!fleet.contains("ghost"));
    }

    #[test]
    fn a_multi_pool_config_warns_that_the_form_edits_one_pool() {
        let (cfg, root) = test_cfg("multipool");
        let dir = root.join("bot-multi");
        let corridor = setup::find_corridor("cngn-usdt-bsc").unwrap();
        setup::write_config(
            &dir,
            corridor,
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        // Duplicate the pool block so the config has two.
        let text = std::fs::read_to_string(dir.join("stitch.toml")).unwrap();
        let pool_start = text.find("[[pools]]").unwrap();
        let doubled = format!("{text}\n{}", &text[pool_start..]);
        std::fs::write(dir.join("stitch.toml"), &doubled).unwrap();

        let fleet = discover(&[], &cfg);
        let bot = fleet.get("bot-multi").unwrap();
        assert_eq!(bot.config.as_ref().unwrap().pools, 2);
        assert!(bot
            .warnings
            .iter()
            .any(|w| matches!(w, Warning::MultiPool { pools: 2 })));
        std::fs::remove_dir_all(&root).ok();
    }
}

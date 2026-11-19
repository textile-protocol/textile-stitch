// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Recreate a bot container on another image, putting the old one back if any
//! step fails.
//!
//! The sequence is generic over [`SwapOps`] so every failure point can be tested
//! against a fake, and the real backend ([`super::BollardDocker`]) only supplies
//! the primitive daemon calls.
//!
//! Both containers are tracked by id, not name, from the moment they exist. A
//! rename that fails, or that succeeds but whose reply is lost, leaves the name
//! pointing at an unknown container; the id doesn't move. So recovery removes
//! the replacement by id and renames and starts the old container by id,
//! whatever names they ended up with.
//!
//! A failed swap reports what it left behind, because the panel holds the bot's
//! wallet claim across the swap and needs to know whether something may still be
//! starting on that wallet:
//!
//! - [`SwapOutcome::Unchanged`]: the old container was never stopped.
//! - [`SwapOutcome::Restored`]: the old container is back under its name, and
//!   started again if it was up. Its start succeeded, like a plain Start.
//! - anything else (no [`SwapFailed`] in the chain): recovery itself failed, or
//!   the outcome is unknown. The caller must treat the bot as possibly live.

use std::collections::HashMap;

use anyhow::{Context, Result};

use super::ContainerState;

/// Compose labels that say which service of which project a container is. A
/// container cloned by a swap carries its original's labels, so these identify
/// one bot across swaps.
const OWNER_LABELS: [&str; 2] = ["com.docker.compose.project", "com.docker.compose.service"];
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// What a failed swap left in place. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SwapOutcome {
    Unchanged,
    Restored,
}

/// A swap failure whose end state is known. Found in an error chain with
/// [`swap_outcome`].
#[derive(Debug, thiserror::Error)]
#[error("{detail}")]
pub struct SwapFailed {
    pub outcome: SwapOutcome,
    pub detail: String,
}

/// The known end state of a failed swap, or `None` if it isn't known.
pub fn swap_outcome(e: &anyhow::Error) -> Option<SwapOutcome> {
    e.chain()
        .find_map(|c| c.downcast_ref::<SwapFailed>())
        .map(|f| f.outcome)
}

pub fn failed(outcome: SwapOutcome, e: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(SwapFailed {
        outcome,
        detail: format!("{e:#}"),
    })
}

/// The container being replaced, read once before anything changes.
pub struct Snapshot<T> {
    pub id: String,
    pub state: ContainerState,
    pub labels: HashMap<String, String>,
    /// Whatever the backend needs to create an identical container.
    pub template: T,
}

/// A container already holding one of the swap's scratch names.
pub struct Occupant {
    pub state: ContainerState,
    pub labels: HashMap<String, String>,
}

/// Whether `occupant` is a stopped leftover of the bot `old` describes: same
/// Compose project and service, and no process left. Anything else under a
/// scratch name is someone else's container and is never removed.
fn is_own_leftover(occupant: &Occupant, old: &HashMap<String, String>) -> bool {
    occupant.state.is_terminal()
        && OWNER_LABELS.iter().all(|key| {
            old.get(*key)
                .is_some_and(|v| !v.is_empty() && occupant.labels.get(*key) == Some(v))
        })
}

/// The daemon calls a swap is made of. `id` arguments are container ids.
#[async_trait]
pub trait SwapOps: Send + Sync {
    type Template: Send + Sync;

    /// Pull `image` from the registry. Never falls back to a local copy.
    async fn pull(&self, image: &str) -> Result<()>;
    async fn snapshot(&self, name: &str) -> Result<Snapshot<Self::Template>>;
    /// The container called `name`, if there is one.
    async fn occupant(&self, name: &str) -> Result<Option<Occupant>>;
    /// Create a stopped container called `name` from `template` on `image`.
    /// Returns its id.
    async fn create(&self, name: &str, template: &Self::Template, image: &str) -> Result<String>;
    async fn stop(&self, id: &str) -> Result<()>;
    /// Start `id`. Already running counts as success.
    async fn start(&self, id: &str) -> Result<()>;
    async fn rename(&self, id: &str, to: &str) -> Result<()>;
    /// Remove a container by id or name. Already gone counts as success.
    async fn remove(&self, id_or_name: &str, force: bool) -> Result<()>;
    /// The container's current name, without Docker's leading `/`.
    async fn name_of(&self, id: &str) -> Result<String>;
}

/// Replace container `name` with one on `new_image`. See the module docs for
/// what a failure leaves behind.
pub async fn swap_image<O: SwapOps>(ops: &O, name: &str, new_image: &str) -> Result<()> {
    let unchanged = |e: anyhow::Error| {
        failed(
            SwapOutcome::Unchanged,
            e.context(format!("{name} is unchanged")),
        )
    };
    // Strict pull before anything else: a registry failure must leave the
    // running bot exactly as it was.
    ops.pull(new_image).await.map_err(unchanged)?;
    let old = ops.snapshot(name).await.map_err(unchanged)?;
    let was_up = old.state.wants_to_be_up();
    // Skipping the stop is only safe when the old process is gone for good. A
    // paused container would be renamed aside with its process intact, and
    // unpausing it later would run two copies of the bot.
    if !was_up && !old.state.is_terminal() {
        return Err(unchanged(anyhow::anyhow!(
            "{name} is {}; start or stop it before deploying an image",
            old.state.as_str()
        )));
    }

    let next = format!("{name}-next");
    let previous = format!("{name}-previous");
    // A scratch name may hold a stopped leftover of an earlier swap of this
    // bot, which is removed. Anything else there (another bot, a running
    // container, an unrelated workload) is refused, never deleted.
    for scratch in [&next, &previous] {
        let Some(occupant) = ops.occupant(scratch).await.map_err(unchanged)? else {
            continue;
        };
        if !is_own_leftover(&occupant, &old.labels) {
            return Err(unchanged(anyhow::anyhow!(
                "a container called {scratch} already exists and isn't a stopped leftover of \
                 {name}; remove or rename it on the host"
            )));
        }
        ops.remove(scratch, false)
            .await
            .with_context(|| format!("removing the leftover {scratch}"))
            .map_err(unchanged)?;
    }
    let new_id = ops
        .create(&next, &old.template, new_image)
        .await
        .with_context(|| format!("creating the replacement container {next}"))
        .map_err(unchanged)?;

    let steps = async {
        if was_up {
            ops.stop(&old.id).await?;
        }
        ops.rename(&old.id, &previous).await?;
        ops.rename(&new_id, name).await?;
        if was_up {
            ops.start(&new_id)
                .await
                .with_context(|| format!("{new_image} didn't start"))?;
        }
        anyhow::Ok(())
    };
    if let Err(e) = steps.await {
        return Err(recover(ops, name, &old, &new_id, e).await);
    }

    // The new container is live under the bot's name; the swap is done. A
    // leftover old container is removed by the next swap.
    if let Err(e) = ops.remove(&old.id, false).await {
        tracing::warn!(
            error = %format!("{e:#}"),
            "{name} runs {new_image}, but its old container is still there as {previous}"
        );
    }
    Ok(())
}

/// Put the old container back under `name` (and start it, if it was up) after
/// a step failed. Returns the error to report.
async fn recover<O: SwapOps>(
    ops: &O,
    name: &str,
    old: &Snapshot<O::Template>,
    new_id: &str,
    err: anyhow::Error,
) -> anyhow::Error {
    let mut problems = Vec::new();
    // First get the replacement out of the way, whatever name it has now, so it
    // can't hold the bot's name or keep running.
    if let Err(e) = ops.remove(new_id, true).await {
        problems.push(format!("removing the replacement: {e:#}"));
    }
    match ops.name_of(&old.id).await {
        Ok(current) if current == name => {}
        Ok(_) => {
            if let Err(e) = ops.rename(&old.id, name).await {
                problems.push(format!("renaming the old container back: {e:#}"));
            }
        }
        Err(e) => problems.push(format!("finding the old container: {e:#}")),
    }
    if problems.is_empty() && old.state.wants_to_be_up() {
        if let Err(e) = ops.start(&old.id).await {
            problems.push(format!("starting the old container again: {e:#}"));
        }
    }
    if problems.is_empty() {
        return failed(
            SwapOutcome::Restored,
            err.context(format!("{name} is back on its old image")),
        );
    }
    let problems = problems.join("; ");
    tracing::error!("couldn't restore {name} after a failed image swap: {problems}");
    err.context(format!(
        "and {name} couldn't be restored ({problems}); check it on the host"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, Clone, PartialEq)]
    struct Ctr {
        id: String,
        name: String,
        image: String,
        running: bool,
        paused: bool,
        service: String,
    }

    /// An in-memory daemon with one failure armed per operation, by step label.
    #[derive(Default)]
    struct Fake {
        boxes: Mutex<Vec<Ctr>>,
        fail: Mutex<Vec<&'static str>>,
        next_id: Mutex<u32>,
    }

    impl Fake {
        fn with_bot(running: bool) -> Self {
            let f = Self::default();
            f.boxes.lock().unwrap().push(Ctr {
                id: "old".into(),
                name: "bot".into(),
                image: "img@sha256:old".into(),
                running,
                paused: false,
                service: "bot".into(),
            });
            f
        }
        fn arm(&self, step: &'static str) {
            self.fail.lock().unwrap().push(step);
        }
        fn trip(&self, step: &str) -> Result<()> {
            let mut fail = self.fail.lock().unwrap();
            if let Some(i) = fail.iter().position(|s| *s == step) {
                fail.remove(i);
                anyhow::bail!("injected {step} failure");
            }
            Ok(())
        }
        fn find<'a>(boxes: &'a mut [Ctr], key: &str) -> Result<&'a mut Ctr> {
            boxes
                .iter_mut()
                .find(|b| b.id == key || b.name == key)
                .ok_or_else(|| anyhow::anyhow!("no such container {key}"))
        }
        fn state(&self) -> Vec<Ctr> {
            self.boxes.lock().unwrap().clone()
        }
    }

    fn labels_for(service: &str) -> HashMap<String, String> {
        HashMap::from([
            (OWNER_LABELS[0].to_string(), "bots".to_string()),
            (OWNER_LABELS[1].to_string(), service.to_string()),
        ])
    }

    fn state_of(b: &Ctr) -> ContainerState {
        if b.paused {
            ContainerState::Paused
        } else if b.running {
            ContainerState::Running
        } else {
            ContainerState::Exited
        }
    }

    #[async_trait]
    impl SwapOps for Fake {
        type Template = String;
        async fn pull(&self, _image: &str) -> Result<()> {
            self.trip("pull")
        }
        async fn snapshot(&self, name: &str) -> Result<Snapshot<String>> {
            self.trip("snapshot")?;
            let mut boxes = self.boxes.lock().unwrap();
            let b = Self::find(&mut boxes, name)?;
            Ok(Snapshot {
                id: b.id.clone(),
                state: state_of(b),
                labels: labels_for(&b.service),
                template: b.service.clone(),
            })
        }
        async fn occupant(&self, name: &str) -> Result<Option<Occupant>> {
            Ok(self
                .boxes
                .lock()
                .unwrap()
                .iter()
                .find(|b| b.name == name)
                .map(|b| Occupant {
                    state: state_of(b),
                    labels: labels_for(&b.service),
                }))
        }
        async fn create(&self, name: &str, service: &String, image: &str) -> Result<String> {
            self.trip("create")?;
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            let id = format!("new{n}");
            self.boxes.lock().unwrap().push(Ctr {
                id: id.clone(),
                name: name.into(),
                image: image.into(),
                running: false,
                paused: false,
                service: service.clone(),
            });
            Ok(id)
        }
        async fn stop(&self, id: &str) -> Result<()> {
            self.trip("stop")?;
            Self::find(&mut self.boxes.lock().unwrap(), id)?.running = false;
            Ok(())
        }
        async fn start(&self, id: &str) -> Result<()> {
            if id == "old" {
                self.trip("restart-old")?;
            } else {
                self.trip("start")?;
            }
            Self::find(&mut self.boxes.lock().unwrap(), id)?.running = true;
            Ok(())
        }
        async fn rename(&self, id: &str, to: &str) -> Result<()> {
            let step = match to {
                t if t.ends_with("-previous") => "rename-old",
                "bot" if id == "old" => "rename-back",
                _ => "rename-new",
            };
            // `-after` variants act and then report failure: a lost reply.
            let lost = self.trip(&format!("{step}-after")).is_err();
            if !lost {
                self.trip(step)?;
            }
            let mut boxes = self.boxes.lock().unwrap();
            if boxes.iter().any(|b| b.name == to) {
                anyhow::bail!("name {to} is taken");
            }
            Self::find(&mut boxes, id)?.name = to.into();
            if lost {
                anyhow::bail!("connection dropped after {step}");
            }
            Ok(())
        }
        async fn remove(&self, key: &str, _force: bool) -> Result<()> {
            self.trip(&format!("remove:{key}"))?;
            if key == "old" {
                self.trip("remove-old")?;
            } else if key.starts_with("new") {
                self.trip("remove-new")?;
            }
            self.boxes
                .lock()
                .unwrap()
                .retain(|b| b.id != key && b.name != key);
            Ok(())
        }
        async fn name_of(&self, id: &str) -> Result<String> {
            self.trip("name-of")?;
            Ok(Self::find(&mut self.boxes.lock().unwrap(), id)?
                .name
                .clone())
        }
    }

    fn only_old(f: &Fake, running: bool) {
        assert_eq!(
            f.state(),
            vec![Ctr {
                id: "old".into(),
                name: "bot".into(),
                image: "img@sha256:old".into(),
                running,
                paused: false,
                service: "bot".into(),
            }]
        );
    }

    #[tokio::test]
    async fn a_clean_swap_leaves_only_the_new_container_running_under_the_name() {
        let f = Fake::with_bot(true);
        swap_image(&f, "bot", "img@sha256:new").await.unwrap();
        let state = f.state();
        assert_eq!(state.len(), 1, "{state:?}");
        assert_eq!(state[0].name, "bot");
        assert_eq!(state[0].image, "img@sha256:new");
        assert!(state[0].running);
    }

    #[tokio::test]
    async fn a_paused_bot_is_refused_before_anything_changes() {
        let f = Fake::with_bot(true);
        f.boxes.lock().unwrap()[0].paused = true;
        let before = f.state();
        let err = swap_image(&f, "bot", "img@sha256:new").await.unwrap_err();
        assert_eq!(swap_outcome(&err), Some(SwapOutcome::Unchanged), "{err:#}");
        assert!(format!("{err:#}").contains("paused"), "{err:#}");
        assert_eq!(f.state(), before);
    }

    #[tokio::test]
    async fn a_stopped_bot_is_swapped_and_left_stopped() {
        let f = Fake::with_bot(false);
        swap_image(&f, "bot", "img@sha256:new").await.unwrap();
        let state = f.state();
        assert_eq!(state.len(), 1);
        assert!(!state[0].running);
    }

    #[tokio::test]
    async fn failures_before_the_stop_leave_the_bot_unchanged() {
        for step in ["pull", "snapshot", "create"] {
            let f = Fake::with_bot(true);
            f.arm(step);
            let err = swap_image(&f, "bot", "img@sha256:new").await.unwrap_err();
            assert_eq!(swap_outcome(&err), Some(SwapOutcome::Unchanged), "{step}");
            only_old(&f, true);
        }
    }

    #[tokio::test]
    async fn every_failure_after_the_stop_puts_the_old_container_back_running() {
        for step in [
            "stop",
            "rename-old",
            "rename-old-after",
            "rename-new",
            "rename-new-after",
            "start",
        ] {
            let f = Fake::with_bot(true);
            f.arm(step);
            let err = swap_image(&f, "bot", "img@sha256:new").await.unwrap_err();
            assert_eq!(
                swap_outcome(&err),
                Some(SwapOutcome::Restored),
                "{step}: {err:#}"
            );
            only_old(&f, true);
        }
    }

    #[tokio::test]
    async fn a_stopped_bot_is_restored_stopped() {
        for step in ["rename-old", "rename-new", "rename-new-after"] {
            let f = Fake::with_bot(false);
            f.arm(step);
            let err = swap_image(&f, "bot", "img@sha256:new").await.unwrap_err();
            assert_eq!(swap_outcome(&err), Some(SwapOutcome::Restored), "{step}");
            only_old(&f, false);
        }
    }

    fn add(f: &Fake, id: &str, name: &str, service: &str, running: bool) {
        f.boxes.lock().unwrap().push(Ctr {
            id: id.into(),
            name: name.into(),
            image: "img@sha256:older".into(),
            running,
            paused: false,
            service: service.into(),
        });
    }

    #[tokio::test]
    async fn a_stopped_leftover_of_the_same_bot_is_cleared_and_the_swap_goes_ahead() {
        for scratch in ["bot-next", "bot-previous"] {
            let f = Fake::with_bot(true);
            add(&f, "stale", scratch, "bot", false);
            swap_image(&f, "bot", "img@sha256:new").await.unwrap();
            let state = f.state();
            assert_eq!(state.len(), 1, "{scratch}: {state:?}");
            assert_eq!(state[0].image, "img@sha256:new");
        }
    }

    #[tokio::test]
    async fn a_scratch_name_held_by_anything_else_is_refused_and_left_alone() {
        for (scratch, service, running) in [
            ("bot-next", "other-bot", false),
            ("bot-previous", "other-bot", false),
            ("bot-previous", "bot", true),
        ] {
            let f = Fake::with_bot(true);
            add(&f, "theirs", scratch, service, running);
            let before = f.state();
            let err = swap_image(&f, "bot", "img@sha256:new").await.unwrap_err();
            assert_eq!(swap_outcome(&err), Some(SwapOutcome::Unchanged), "{err:#}");
            assert!(format!("{err:#}").contains("already exists"), "{err:#}");
            assert_eq!(f.state(), before, "{scratch}/{service}/{running}");
        }
    }

    #[tokio::test]
    async fn a_leftover_that_cannot_be_removed_leaves_the_bot_unchanged() {
        let f = Fake::with_bot(true);
        add(&f, "stale", "bot-previous", "bot", false);
        f.arm("remove:bot-previous");
        let before = f.state();
        let err = swap_image(&f, "bot", "img@sha256:new").await.unwrap_err();
        assert_eq!(swap_outcome(&err), Some(SwapOutcome::Unchanged), "{err:#}");
        assert_eq!(f.state(), before);
    }

    #[tokio::test]
    async fn a_bot_without_compose_labels_never_has_a_leftover_removed() {
        let old = HashMap::new();
        let occupant = Occupant {
            state: ContainerState::Exited,
            labels: HashMap::new(),
        };
        assert!(!is_own_leftover(&occupant, &old));
    }

    #[tokio::test]
    async fn a_failed_recovery_reports_no_known_outcome() {
        for (step, recovery) in [
            ("start", "remove-new"),
            ("rename-old-after", "rename-back"),
            ("start", "restart-old"),
            ("rename-new", "name-of"),
        ] {
            let f = Fake::with_bot(true);
            f.arm(step);
            f.arm(recovery);
            let err = swap_image(&f, "bot", "img@sha256:new").await.unwrap_err();
            assert_eq!(swap_outcome(&err), None, "{step}/{recovery}: {err:#}");
            assert!(
                format!("{err:#}").contains("couldn't be restored"),
                "{err:#}"
            );
        }
    }

    #[tokio::test]
    async fn failing_to_remove_the_old_container_after_a_good_swap_is_not_an_error() {
        let f = Fake::with_bot(true);
        f.arm("remove-old");
        swap_image(&f, "bot", "img@sha256:new").await.unwrap();
        let live: Vec<_> = f.state().into_iter().filter(|b| b.running).collect();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].name, "bot");
        assert_eq!(live[0].image, "img@sha256:new");
    }
}

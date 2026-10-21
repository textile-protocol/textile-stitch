// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fleet opt-in lease for the hedge executor. Disabled bots perform no perps
//! I/O. The service owns GMX execution; this task has no new signing authority.
use crate::config::Config;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PerpsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub assignment_id: String,
    #[serde(default)]
    pub bot_id: String,
    #[serde(default)]
    pub revision: String,
}
impl PerpsConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        anyhow::ensure!(
            (8..=64).contains(&self.assignment_id.len())
                && self
                    .assignment_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-'),
            "FX hedge assignment required"
        );
        anyhow::ensure!(
            !self.bot_id.is_empty() && self.bot_id.len() <= 128 && self.revision.len() == 36,
            "Enable FX hedging from Fleet to get an authorization revision"
        );
        Ok(())
    }
}

pub async fn request(
    cfg: &Config,
    dir: &Path,
    method: reqwest::Method,
    id: &str,
    suffix: &str,
    body: Option<Value>,
) -> anyhow::Result<Value> {
    anyhow::ensure!(
        (8..=64).contains(&id.len()) && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'),
        "Invalid assignment ID"
    );
    let rfq = cfg
        .rfq
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Connect this Stitch to Textile first"))?;
    let key = crate::rfq::load_rfq_api_key(&rfq.api_key_env, Some(dir))?;
    let origin = crate::venue::enroll::venue_origin_from_config(cfg, None);
    let url = format!(
        "{}/v2/maker/perps/{id}{suffix}",
        origin.trim_end_matches('/')
    );
    crate::config::assert_feed_url(&url, "FX hedge service")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let request = client.request(method, url).bearer_auth(key);
    let request = if let Some(body) = body {
        request.json(&body)
    } else {
        request
    };
    let response = request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("FX hedge service unavailable"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "FX hedge request refused ({})",
        response.status()
    );
    let value: Value = response.json().await?;
    value
        .get("data")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Invalid FX hedge response"))
}

pub fn maybe_spawn(
    cfg: &Config,
    dry_run: bool,
    config_path: &Path,
) -> Option<tokio::task::JoinHandle<()>> {
    let perps = cfg
        .perps
        .as_ref()
        .filter(|p| p.enabled && !dry_run)?
        .clone();
    let cfg = cfg.clone();
    let dir = config_path.parent()?.to_owned();
    let config_path = config_path.to_owned();
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            // Read the saved gate as well as the startup snapshot. A Fleet off
            // survives a failed restart or an unavailable venue API.
            let authorized = std::fs::read_to_string(&config_path)
                .ok()
                .and_then(|raw| Config::from_toml(&raw).ok())
                .and_then(|current| current.perps)
                .is_some_and(|current| lease_matches(&current, &perps));
            if !authorized {
                break;
            }
            if request(
                &cfg,
                &dir,
                reqwest::Method::POST,
                &perps.assignment_id,
                "/heartbeat",
                Some(json!({"revision": perps.revision, "botId": perps.bot_id})),
            )
            .await
            .is_err()
            {
                tracing::warn!("FX hedge authorization unavailable; no new hedge once the 15-second lease expires");
            }
        }
    }))
}

fn lease_matches(saved: &PerpsConfig, running: &PerpsConfig) -> bool {
    saved.enabled
        && saved.assignment_id == running.assignment_id
        && saved.bot_id == running.bot_id
        && saved.revision == running.revision
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absent_off_and_dry_run_never_start_a_heartbeat() {
        let cfg: Config = toml::from_str(include_str!("../../stitch.example.toml")).unwrap();
        assert!(maybe_spawn(&cfg, false, Path::new("/unused/config.toml")).is_none());
        let mut cfg = cfg;
        cfg.perps = Some(PerpsConfig {
            enabled: true,
            ..Default::default()
        });
        assert!(maybe_spawn(&cfg, true, Path::new("/unused/config.toml")).is_none());
        cfg.perps.as_mut().unwrap().enabled = false;
        assert!(maybe_spawn(&cfg, false, Path::new("/unused/config.toml")).is_none());
    }
    #[test]
    fn saved_disable_and_revision_change_stop_the_old_process() {
        let active = PerpsConfig {
            enabled: true,
            assignment_id: "celo-idrx".into(),
            bot_id: "one".into(),
            revision: "a".into(),
        };
        assert!(lease_matches(&active, &active));
        let off = PerpsConfig {
            enabled: false,
            ..active.clone()
        };
        assert!(!lease_matches(&off, &active));
        let newer = PerpsConfig {
            revision: "b".into(),
            ..active.clone()
        };
        assert!(!lease_matches(&newer, &active));
    }
    #[test]
    fn off_needs_no_assignment_but_on_needs_fleet_authorization() {
        assert!(PerpsConfig::default().validate().is_ok());
        assert!(PerpsConfig {
            enabled: true,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(PerpsConfig {
            enabled: true,
            assignment_id: "../bad/id".into(),
            bot_id: "bot".into(),
            revision: "x".repeat(36)
        }
        .validate()
        .is_err());
    }
}

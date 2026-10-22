// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The panel's half of the change log: every config the panel writes goes
//! through [`write_config`], which records the change under whoever is
//! signed in. See [`crate::changelog`] for the rest.
//!
//! The panel never ships records anywhere. They land in the bot's journal,
//! and the bot ships them if its operator opted in.

use std::path::Path;

use crate::changelog::{self, Origin, Source};
use crate::panel::auth::Identity;

use super::{ApiError, AppState};

/// The actor for a password session. The password is shared, so the change
/// log can say a password holder did it and nothing more. Panels that need
/// a person on every record sign in through the tailnet instead.
pub const PASSWORD_ACTOR: &str = "panel password (shared)";

/// Who a signed-in request is, as the change log writes it.
pub fn actor(identity: &Identity) -> String {
    match identity {
        Identity::Tailnet(login) => format!("tailnet:{login}"),
        Identity::Password => PASSWORD_ACTOR.to_string(),
    }
}

/// A panel action by `identity` on the bot called `bot`.
pub fn origin(identity: &Identity, source: Source, bot: &str) -> Origin {
    Origin {
        source,
        actor: actor(identity),
        bot: Some(bot.to_string()),
    }
}

/// The same actor, recorded under a different path. A failed live restart
/// puts the old config back on the operator's behalf, and that is a change
/// of its own.
pub fn as_rollback(origin: &Origin) -> Origin {
    Origin {
        source: Source::PanelRollback,
        ..origin.clone()
    }
}

/// Write a bot's config atomically and record what changed.
///
/// Errors only when the config itself couldn't be written. A change log that
/// couldn't record comes back as `Ok(Some(warning))` for the caller to show:
/// the save happened, and the bot will record it as a host edit when it next
/// starts. Files the log creates are handed to the bot's uid as they're
/// opened, or the bot couldn't append to its own journal.
pub fn write_config(
    state: &AppState,
    path: &Path,
    toml: &str,
    origin: &Origin,
) -> Result<Option<String>, ApiError> {
    let recorded = changelog::write_config_recorded(
        path,
        toml,
        origin,
        crate::time::unix_now_ms(),
        Some(state.cfg.bot_uid),
    )
    .map_err(|e| ApiError::internal(&e))?;
    Ok(recorded.warning)
}

/// Record a config the panel wrote by other means (the wizard writes a whole
/// bot directory at once) as a change from `before`.
pub fn record_written(
    state: &AppState,
    path: &Path,
    before: Option<&str>,
    after: &str,
    origin: &Origin,
) -> Option<String> {
    let recorded =
        changelog::journal::Journal::open(path, Some(state.cfg.bot_uid)).and_then(|journal| {
            changelog::journal::record(&journal, before, after, origin, crate::time::unix_now_ms())
        });
    recorded.err().map(|e| {
        tracing::error!(path = %path.display(), "couldn't record the new config: {e:#}");
        format!(
            "The change log couldn't record this config ({e:#}). The bot records its values as \
             a baseline when it first starts."
        )
    })
}

/// Before anything replaces the bot's maker key: deliver the change records
/// it owes Textile under the key it has now. The API files a record under
/// whichever maker's key ships it, so records still queued at the switch
/// would land under the new maker. Refused, with nothing changed, when they
/// can't go out. Only bots that ship have anything owed, so a panel whose
/// bots don't opt in never makes this call.
pub async fn deliver_before_key_change(path: &Path) -> Result<(), ApiError> {
    changelog::ship::deliver_owed(path)
        .await
        .map(|_| ())
        .map_err(|e| {
            ApiError::conflict(format!(
                "This bot has parameter-change records Textile hasn't received yet, and they \
                 have to go out under its current maker before its key changes ({e:#}). \
                 Nothing was changed; try again once Textile is reachable."
            ))
        })
}

/// Append a change-log warning to a save's response message.
pub fn with_warning(mut body: serde_json::Value, warning: Option<&str>) -> serde_json::Value {
    let (Some(obj), Some(warning)) = (body.as_object_mut(), warning) else {
        return body;
    };
    let message = obj
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_string();
    obj.insert(
        "message".to_string(),
        serde_json::Value::String(format!("{message} {warning}").trim().to_string()),
    );
    obj.insert(
        "changeLogWarning".to_string(),
        serde_json::Value::String(warning.to_string()),
    );
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tailnet_login_names_the_person_and_a_password_says_it_cannot() {
        assert_eq!(
            actor(&Identity::Tailnet("ops@textile.example".into())),
            "tailnet:ops@textile.example"
        );
        assert_eq!(actor(&Identity::Password), PASSWORD_ACTOR);
    }

    #[test]
    fn a_rollback_keeps_the_actor_and_changes_the_path() {
        let o = origin(&Identity::Password, Source::PanelSettings, "acme");
        let r = as_rollback(&o);
        assert_eq!(r.actor, o.actor);
        assert_eq!(r.bot, o.bot);
        assert_eq!(r.source, Source::PanelRollback);
    }

    #[test]
    fn a_warning_is_appended_to_the_message_and_kept_on_its_own() {
        let body = with_warning(
            serde_json::json!({ "message": "Saved." }),
            Some("Log failed."),
        );
        assert_eq!(body["message"], "Saved. Log failed.");
        assert_eq!(body["changeLogWarning"], "Log failed.");
        let untouched = with_warning(serde_json::json!({ "message": "Saved." }), None);
        assert!(untouched.get("changeLogWarning").is_none());
    }
}

/// End to end: each panel route that writes a config, through the router,
/// read back from the bot's journal.
#[cfg(test)]
mod route_tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use super::super::testkit::{harness, Harness, TEST_KEY};
    use super::PASSWORD_ACTOR;
    use crate::changelog::journal::read_all;
    use crate::changelog::{ChangeRecord, HOST_EDIT_ACTOR};
    use crate::panel::docker::fake::{container, dir_layout_mounts};
    use crate::panel::docker::ContainerState;
    use crate::panel::naming::LABEL_BOT;

    fn config_path(h: &Harness, name: &str) -> std::path::PathBuf {
        h.root.join(name).join("stitch.toml")
    }

    /// A running panel bot on `corridor`, as the wizard would leave it.
    fn seed(h: &Harness, name: &str, corridor: &str) {
        crate::setup::write_config(
            h.root.join(name),
            crate::setup::find_corridor(corridor).unwrap(),
            TEST_KEY,
        )
        .unwrap();
        let mut c = container(&format!("stitch-{name}"), ContainerState::Running);
        c.image = h.state.cfg.bot_image.clone();
        c.labels.insert(LABEL_BOT.to_string(), name.to_string());
        c.mounts = dir_layout_mounts(&h.root.join(name).display().to_string());
        h.docker.add_container(c);
        h.docker.set_image_label(
            &format!("sha256:id-stitch-{name}"),
            crate::panel::naming::LABEL_RFQ_RESERVATIONS,
            crate::panel::naming::RFQ_RESERVATIONS_TOKEN,
        );
    }

    /// Records written by `source`, in order.
    fn from(h: &Harness, name: &str, source: &str) -> Vec<ChangeRecord> {
        read_all(&config_path(h, name))
            .unwrap()
            .into_iter()
            .filter(|r| r.source == source)
            .collect()
    }

    fn one<'a>(records: &'a [ChangeRecord], parameter: &str) -> &'a ChangeRecord {
        let found: Vec<_> = records
            .iter()
            .filter(|r| r.parameter == parameter)
            .collect();
        assert_eq!(found.len(), 1, "{parameter}: {records:?}");
        found[0]
    }

    async fn current_buy_spread(h: &Harness, name: &str) -> (String, String) {
        let (_, body) = h.get(&format!("/api/bots/{name}/settings")).await;
        let v = Harness::parse(&body);
        (
            v["buy"]["kind"].as_str().unwrap().to_string(),
            v["buy"]["value"].as_str().unwrap().to_string(),
        )
    }

    #[tokio::test]
    async fn a_settings_save_is_recorded_with_the_operator_and_both_values() {
        let h = harness("changes-settings");
        seed(&h, "bot-a", "cngn-usdt-bsc");
        let (kind, before) = current_buy_spread(&h, "bot-a").await;
        let after = if kind == "bps" { "77" } else { "7.7" };
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "buy": { "kind": kind, "value": after } }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            Harness::parse(&body).get("changeLogWarning").is_none(),
            "{body}"
        );

        // The first touch also writes the baseline it was compared against.
        assert!(!from(&h, "bot-a", "baseline").is_empty());
        let saved = from(&h, "bot-a", "panel.settings");
        let r = one(&saved, &format!("buy_offset_{kind}"));
        assert_eq!(r.actor, PASSWORD_ACTOR);
        assert_eq!(r.bot.as_deref(), Some("bot-a"));
        assert_eq!(r.before.as_deref(), Some(before.as_str()));
        assert_eq!(r.after.as_deref(), Some(after));
        assert!(r.scope.starts_with("pool:0x"), "{r:?}");
        assert!(r.changed_at.ends_with('Z'));
        assert_eq!(saved.len(), 1, "only the field that moved: {saved:?}");
    }

    #[tokio::test]
    async fn a_tailnet_operator_is_named_on_the_record() {
        let mut h = harness("changes-tailnet");
        let mut cfg = (*h.state.cfg).clone();
        cfg.auth = crate::panel::AuthMode::Either {
            allowed_users: vec!["ops@textile.example".into()],
            hash: "$argon2id$test".into(),
        };
        h.state.cfg = std::sync::Arc::new(cfg);
        h.app = super::super::router(h.state.clone());
        seed(&h, "bot-a", "cngn-usdt-bsc");
        let (kind, _) = current_buy_spread(&h, "bot-a").await;
        let (status, body) = h
            .send(
                axum::http::Request::builder()
                    .method("PATCH")
                    .uri("/api/bots/bot-a/settings")
                    .header("content-type", "application/json")
                    .header("tailscale-user-login", "ops@textile.example")
                    .body(axum::body::Body::from(
                        json!({ "buy": { "kind": kind, "value": "66" } }).to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let saved = from(&h, "bot-a", "panel.settings");
        assert_eq!(saved[0].actor, "tailnet:ops@textile.example");
    }

    #[tokio::test]
    async fn a_raw_save_records_every_parameter_it_moved() {
        let h = harness("changes-raw");
        seed(&h, "bot-a", "cngn-usdt-bsc");
        let (_, body) = h.get("/api/bots/bot-a/config").await;
        let toml = Harness::parse(&body)["toml"].as_str().unwrap().to_string();
        let edited = format!("{toml}\n[price_guard]\nmax_loss_bps = 700\nmax_jump_bps = 400\n");
        let (status, body) = h
            .put_json("/api/bots/bot-a/config", json!({ "toml": edited }))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let saved = from(&h, "bot-a", "panel.raw");
        assert_eq!(saved.len(), 2, "{saved:?}");
        let loss = one(&saved, "max_loss_bps");
        assert_eq!(loss.before.as_deref(), Some("1500"));
        assert_eq!(loss.after.as_deref(), Some("700"));
        assert_eq!(one(&saved, "max_jump_bps").after.as_deref(), Some("400"));
        assert_eq!(saved[0].change_set_id, saved[1].change_set_id);
    }

    #[tokio::test]
    async fn adding_and_removing_a_pool_are_recorded_as_values_appearing_and_going() {
        let h = harness("changes-pools");
        seed(&h, "bot-a", "cngn-usdt-celo");
        let (status, body) = h
            .post_json(
                "/api/bots/bot-a/pools",
                json!({ "corridorId": "wbrl-usdt-celo" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let added = from(&h, "bot-a", "panel.pools.add");
        assert!(!added.is_empty());
        assert!(added
            .iter()
            .all(|r| r.before.is_none() && r.after.is_some()));
        let scope = added[0].scope.clone();
        assert!(added.iter().all(|r| r.scope == scope));
        assert!(added.iter().any(|r| r.parameter == "feed_url"));

        let pool = Harness::parse(&body)["settings"]["pools"][1].clone();
        let (status, body) = h
            .delete(&format!(
                "/api/bots/bot-a/pools/1?collateral={}&debt={}",
                pool["collateral"].as_str().unwrap(),
                pool["debt"].as_str().unwrap()
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let removed = from(&h, "bot-a", "panel.pools.remove");
        assert_eq!(removed.len(), added.len(), "{removed:?}");
        assert!(removed.iter().all(|r| r.scope == scope));
        assert!(removed
            .iter()
            .all(|r| r.before.is_some() && r.after.is_none()));
    }

    /// The modules routes need an RFQ-connected, single-pool vault bot.
    fn seed_modules(h: &Harness, enabled: bool) {
        seed(h, "bot-a", "cngn-usdt-bsc");
        crate::setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_change_tests").unwrap();
        let path = config_path(h, "bot-a");
        let mut doc = std::fs::read_to_string(&path)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        doc["modules_enabled"] = toml_edit::value(enabled);
        doc["vault"] = toml_edit::table();
        doc["vault"]["address"] = toml_edit::value("0x00000000000000000000000000000000000000aa");
        doc["rfq"] = toml_edit::table();
        doc["rfq"]["url"] = toml_edit::value("wss://localhost/v2/maker/stream");
        doc["rfq"]["maker_id"] = toml_edit::value("test");
        doc["rfq"]["validation_contract"] =
            toml_edit::value("0x00000000000000000000000000000000000000bb");
        doc["rfq"]["enabled"] = toml_edit::value(true);
        std::fs::write(path, doc.to_string()).unwrap();
    }

    #[tokio::test]
    async fn turning_modules_on_records_the_switch_and_the_settings_it_brings() {
        let h = harness("changes-modules-on");
        seed_modules(&h, false);
        let (status, body) = h
            .put_json(
                "/api/bots/bot-a/modules/enabled",
                json!({ "enabled": true }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let saved = from(&h, "bot-a", "panel.modules.enabled");
        let flag = one(&saved, "modules_enabled");
        assert_eq!(flag.before.as_deref(), Some("false"));
        assert_eq!(flag.after.as_deref(), Some("true"));
        assert!(saved
            .iter()
            .any(|r| r.parameter == "modules.spreads.max_extra_bps" && r.before.is_none()));
    }

    #[tokio::test]
    async fn a_modules_save_records_the_module_settings_it_moved() {
        let h = harness("changes-modules");
        seed_modules(&h, true);
        let (_, body) = h.get("/api/bots/bot-a/modules").await;
        let view = Harness::parse(&body);
        let mut config = view["config"].clone();
        config["spreads"]["max_extra_bps"] = json!(150);
        let (status, body) = h
            .put_json(
                "/api/bots/bot-a/modules",
                json!({ "revision": view["revision"], "config": config }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let saved = from(&h, "bot-a", "panel.modules");
        assert_eq!(saved.len(), 1, "{saved:?}");
        assert_eq!(saved[0].parameter, "modules.spreads.max_extra_bps");
        assert_eq!(saved[0].before.as_deref(), Some("100"));
        assert_eq!(saved[0].after.as_deref(), Some("150"));
    }

    #[tokio::test]
    async fn a_hand_edit_before_a_panel_save_is_not_pinned_on_the_operator() {
        let h = harness("changes-hand-edit");
        seed(&h, "bot-a", "cngn-usdt-bsc");
        // The panel sees the bot once, so a baseline exists.
        let (kind, _) = current_buy_spread(&h, "bot-a").await;
        let (status, _) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "buy": { "kind": kind, "value": "55" } }),
            )
            .await;
        assert_eq!(status, StatusCode::OK);

        // Someone edits the file over SSH...
        let path = config_path(&h, "bot-a");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            format!("{text}\n[price_guard]\nmax_loss_bps = 900\n"),
        )
        .unwrap();
        // ...and then the operator saves an unrelated spread.
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "buy": { "kind": kind, "value": "44" } }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let edits = from(&h, "bot-a", "host-edit");
        let edit = one(&edits, "max_loss_bps");
        assert_eq!(edit.actor, HOST_EDIT_ACTOR);
        assert_eq!(edit.after.as_deref(), Some("900"));
        assert!(edit.detected_at.is_some());
        let saves = from(&h, "bot-a", "panel.settings");
        assert!(
            saves.iter().all(|r| r.parameter != "max_loss_bps"),
            "{saves:?}"
        );
        assert_eq!(saves.last().unwrap().after.as_deref(), Some("44"));
    }

    #[tokio::test]
    async fn a_bot_the_wizard_creates_starts_its_log_under_the_operator() {
        let h = harness("changes-create");
        let (status, body) = h
            .post_json(
                "/api/bots",
                json!({
                    "name": "bot-a",
                    "corridorId": "cngn-usdt-bsc",
                    "signer": { "kind": "local", "privateKey": TEST_KEY },
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let created = from(&h, "bot-a", "panel.create");
        assert!(!created.is_empty());
        assert!(created
            .iter()
            .all(|r| r.actor == PASSWORD_ACTOR && r.before.is_none()));
        assert!(from(&h, "bot-a", "baseline").is_empty());
        // The bot's first start then has nothing to add.
        let path = config_path(&h, "bot-a");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            crate::changelog::record_startup(&path, &text, 1).unwrap(),
            0
        );
    }

    /// A stand-in for Textile's change-log endpoint answering `status`.
    async fn change_log_api(status: StatusCode) -> String {
        let app = axum::Router::new().route(
            crate::changelog::ship::ENDPOINT_PATH,
            axum::routing::post(move || async move { (status, "{\"data\":{}}") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        origin
    }

    /// A running bot that ships, connected to `origin`, with records owed.
    fn seed_shipping(h: &Harness, origin: &str) -> std::path::PathBuf {
        seed(h, "bot-a", "cngn-usdt-bsc");
        let path = config_path(h, "bot-a");
        let text = std::fs::read_to_string(&path).unwrap();
        let text = format!(
            "{text}\n[change_log]\nship = true\n\n[rfq]\nenabled = false\nurl = \"{}/v2/maker/stream\"\n\
             maker_id = \"old-maker\"\nvalidation_contract = \"0x00000000000000000000000000000000000000bb\"\n",
            origin.replace("http://", "ws://")
        );
        std::fs::write(&path, &text).unwrap();
        crate::setup::write_rfq_api_key(h.root.join("bot-a"), "tx_live_old_key").unwrap();
        crate::changelog::record_startup(&path, &text, 1).unwrap();
        assert!(crate::changelog::ship::owed(&path) > 0);
        path
    }

    #[tokio::test]
    async fn a_new_key_waits_until_the_old_makers_records_are_delivered() {
        let h = harness("changes-key-blocked");
        let path = seed_shipping(&h, &change_log_api(StatusCode::SERVICE_UNAVAILABLE).await);
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "rfqApiKey": "tx_live_new_key" }),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("current maker"), "{body}");
        let key =
            std::fs::read_to_string(h.root.join("bot-a").join(crate::setup::RFQ_API_KEY_FILE));
        assert_eq!(key.unwrap().trim(), "tx_live_old_key");
        assert!(crate::changelog::ship::owed(&path) > 0);

        let h = harness("changes-key-delivered");
        let path = seed_shipping(&h, &change_log_api(StatusCode::OK).await);
        let (status, body) = h
            .patch_json(
                "/api/bots/bot-a/settings",
                json!({ "rfqApiKey": "tx_live_new_key" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(crate::changelog::ship::owed(&path), 0);
    }

    #[tokio::test]
    async fn deleting_a_shipping_bot_with_unshipped_records_is_refused() {
        let h = harness("changes-delete");
        seed(&h, "bot-a", "cngn-usdt-bsc");
        let path = config_path(&h, "bot-a");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{text}\n[change_log]\nship = true\n")).unwrap();
        let shipping = std::fs::read_to_string(&path).unwrap();
        crate::changelog::record_startup(&path, &shipping, 1).unwrap();
        assert!(crate::changelog::ship::owed(&path) > 0);

        let (status, body) = h
            .send(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/bots/bot-a?deleteConfig=true")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("hasn't received"), "{body}");
        assert!(path.exists());
        assert!(h.docker.exists("stitch-bot-a"));
    }
}

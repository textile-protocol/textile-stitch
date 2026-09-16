// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Permit2 approvals sent by the custodian, for signers that cannot sign a
//! transaction themselves.
//!
//! A Fireblocks bot without raw signing quotes perfectly well — RFQ quotes, the
//! ladder and enrolment are all EIP-712, which `TYPED_MESSAGE` covers — and
//! then cannot send the one-time `ERC20.approve(Permit2, …)` that makes its
//! orders fillable, because a transaction hash is not typed data. Until now the
//! wizard greyed the button and told the operator to go and do it by hand in
//! the Fireblocks console.
//!
//! It does not have to be by hand. `CONTRACT_CALL` is a first-class Fireblocks
//! operation — distinct from `RAW`, and needing none of raw signing's paid
//! entitlement — in which Fireblocks builds, nonces, signs and broadcasts the
//! call itself. The panel already holds this bot's API key and RSA key (it
//! writes them, and reads them back for Connect), so it can ask for the approve
//! directly. The operator adds a Contract Call policy rule and never leaves the
//! wizard.
//!
//! The inversion is why this lives here and not behind [`crate::signer::Signer`]:
//! everywhere else the bot builds a transaction and signs its hash, and the
//! caller ends up holding a signature. Here Fireblocks ends up holding the
//! transaction and the caller gets a hash back. There is nowhere in the trait
//! to put that, and `sign_digest` stays correctly refused — the taker and
//! closer legs still need raw signing, and [`crate::signer::SignerConfig::raw_signing_unavailable`]
//! still says so.
//!
//! **This is not a general "call a contract" route.** The destination is
//! restricted to a token the bot's own `[[pools]]` name, the spender is the
//! config's Permit2 and nothing else, and the calldata is built here rather
//! than accepted from the caller. A panel session is an operator, but an
//! operator asking for an approval should not be able to reach an arbitrary
//! contract with the workspace's key by editing a request body.

use alloy_primitives::Address;
use axum::extract::{Path as UrlPath, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use super::allowances::read_allowance;
use super::settings::config_path;
use super::{ApiError, AppState};
use crate::chain::approve::{approval_action, required_approvals, ApprovalAction, ApprovalMode};
use crate::chain::rpc::Rpc;
use crate::closer::executor::encode_approve;
use crate::config::Config;
use crate::signer::fireblocks::FireblocksSigner;
use crate::signer::SignerConfig;

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustodyApproveBody {
    /// The ERC-20 to approve. Must be a token this bot's pools trade — see the
    /// module docs on why this is not free-form.
    pub token: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustodyApproveResponse {
    pub token: String,
    /// The Permit2 the allowance was granted to, echoed so the operator can
    /// check it against the config without a second call.
    pub spender: String,
    /// `null` when the allowance already covered the commitment and nothing was
    /// sent — re-clicking after a successful approve is not an error.
    pub tx_hash: Option<String>,
    /// The Fireblocks transaction id, for finding the call in the console.
    pub fireblocks_tx_id: Option<String>,
    /// Submit to mined, in milliseconds. `null` when nothing was sent.
    pub elapsed_ms: Option<u64>,
    /// True when this call sent nothing because the allowance was already good.
    pub already_approved: bool,
}

/// `POST /api/bots/{name}/approve/custody`
///
/// One token per call. The wizard sends two approvals on a two-sided corridor
/// and wants to show them landing one at a time; a single request that did both
/// would either report nothing until the second mined, or need a stream to say
/// anything useful in between.
pub async fn approve_via_custody(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<CustodyApproveBody>,
) -> Result<Response, ApiError> {
    let token: Address = body
        .token
        .trim()
        .parse()
        .map_err(|_| ApiError::bad_request(format!("{:?} is not an address", body.token)))?;

    let (bot, _fleet) = state.bot_and_fleet(&name).await?;
    super::require_editable(&bot)?;
    // Validate against the read we have before claiming anything: a bot on the
    // wrong signer, or an address this config never trades, should be a clean
    // refusal rather than a wallet held for the length of one.
    plan_for(&read_config(&bot)?, &name, token)?;

    // Hold the wallet for the send, exactly as the container-run approval does
    // — but on the nonce half of the check only. The signer half is the thing
    // this route exists to route around, and running it here would refuse every
    // call with the very message the operator clicked the button to resolve.
    // The race itself is still real: Fireblocks reads the pending nonce off the
    // chain like anything else, and a sibling bot on the same wallet with a
    // different signer can be spending it.
    let (bot, claim) =
        super::logs::reserve_approval(&state, &name, bot, super::logs::approve_wallet_check)
            .await?;

    // Re-read under the claim and send from *that*, never from the read above.
    // `reserve_approval` pins the wallet across a config save, not the pools:
    // a save that landed in between could have moved the corridor onto other
    // tokens, and approving the ones we looked at first would grant an
    // allowance the bot no longer has any use for.
    let path = config_path(&bot)?;
    let cfg = read_config(&bot)?;
    let plan = plan_for(&cfg, &name, token)?;

    let rpc = Rpc::new(cfg.rpc_url.clone());
    let allowance = read_allowance(&rpc, token, plan.owner, plan.permit2)
        .await
        .map_err(|e| ApiError::conflict(format!("couldn't read the current allowance: {e:#}")))?;
    let amount = match approval_action(
        allowance,
        plan.required,
        plan.uses_max_liquidity,
        ApprovalMode::Max,
    ) {
        // Not an error: the wizard sends one request per token and an operator
        // can click twice, so "it was already done" is a normal answer.
        ApprovalAction::AlreadyApproved => {
            return Ok(Json(CustodyApproveResponse {
                token: format!("{token:#x}"),
                spender: format!("{:#x}", plan.permit2),
                tx_hash: None,
                fireblocks_tx_id: None,
                elapsed_ms: None,
                already_approved: true,
            })
            .into_response());
        }
        ApprovalAction::Approve(amount) => amount,
    };

    let signer =
        FireblocksSigner::from_config_with(&plan.fireblocks, &super::enroll::secrets_beside(&path))
            .map_err(|e| {
                ApiError::conflict(format!(
                    "couldn't build a Fireblocks client from {name}'s config: {e:#}"
                ))
            })?;

    let asset_id = signer
        .evm_asset_id(cfg.chain_id)
        .await
        .map_err(|e| ApiError::conflict(format!("{e:#}")))?;

    tracing::info!(
        bot = %name,
        token = %format!("{token:#x}"),
        %asset_id,
        "sending a Permit2 approval through Fireblocks"
    );
    let sent = signer
        .contract_call(
            &asset_id,
            token,
            &encode_approve(plan.permit2, amount),
            &format!("Textile Stitch: Permit2 approval for {name}"),
        )
        .await
        .map_err(|e| ApiError::conflict(format!("{e:#}")))?;
    // Held until here on purpose: the claim covers the broadcast, not just the
    // request that started it.
    drop(claim);

    Ok(Json(CustodyApproveResponse {
        token: format!("{token:#x}"),
        spender: format!("{:#x}", plan.permit2),
        tx_hash: Some(sent.tx_hash),
        fireblocks_tx_id: Some(sent.id),
        elapsed_ms: Some(sent.elapsed.as_millis().min(u128::from(u64::MAX)) as u64),
        already_approved: false,
    })
    .into_response())
}

/// This bot's `stitch.toml`, parsed.
fn read_config(bot: &crate::panel::inventory::Bot) -> Result<Config, ApiError> {
    let path = config_path(bot)?;
    let toml = std::fs::read_to_string(&path).map_err(|e| {
        ApiError::internal(&anyhow::anyhow!(e).context(format!("reading {}", path.display())))
    })?;
    Config::from_toml(&toml).map_err(ApiError::bad_request)
}

/// Everything the send needs, taken from one read of the config.
///
/// Owns its values rather than borrowing the `Config` so the caller can do this
/// twice — once to refuse early, once under the wallet claim — without keeping
/// the first parse alive.
#[cfg_attr(test, derive(Debug))]
struct ApprovalPlan {
    fireblocks: crate::signer::FireblocksConfig,
    permit2: Address,
    /// The wallet the allowance is read for: the vault account's address.
    owner: Address,
    /// Committed liquidity for this token, in its atomic units.
    required: alloy_primitives::U256,
    uses_max_liquidity: bool,
}

/// Validate that this bot can approve this token through its custodian.
///
/// Every refusal the route can give before it touches the network, in the order
/// that gives the most useful message.
fn plan_for(cfg: &Config, name: &str, token: Address) -> Result<ApprovalPlan, ApiError> {
    let fireblocks = match cfg.signer.as_ref() {
        Some(SignerConfig::Fireblocks(c)) => c.clone(),
        _ => {
            return Err(ApiError::conflict(format!(
                "{name} is not on a Fireblocks signer, so there is no custodian to send its \
                 approvals. Run the normal approval instead."
            )))
        }
    };

    // The whole reason this route exists. A Fireblocks bot that already has raw
    // signing can sign its own transactions, and the ordinary approval — which
    // runs in the bot's own image and shares its nonce handling — is the better
    // path for it.
    if fireblocks.raw_signing {
        return Err(ApiError::conflict(format!(
            "{name} has raw signing enabled, so it can send its own approvals. Use the normal \
             approval."
        )));
    }

    let permit2: Address = cfg.permit2.parse().map_err(|e| {
        ApiError::bad_request(format!("the config's permit2 address is invalid: {e}"))
    })?;
    let owner: Address = fireblocks.operator_address.parse().map_err(|e| {
        ApiError::bad_request(format!("the config's operator address is invalid: {e}"))
    })?;

    // Restrict the destination to a token this config actually trades. See the
    // module docs: the calldata is ours, and so is the contract it goes to.
    let required = required_approvals(cfg).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let plan = required.iter().find(|r| r.token == token).ok_or_else(|| {
        ApiError::bad_request(format!(
            "{token:#x} is not a token {name} trades, so it needs no Permit2 approval. Approvals \
             are only sent for the tokens this bot's pools name."
        ))
    })?;

    Ok(ApprovalPlan {
        fireblocks,
        permit2,
        owner,
        required: plan.required,
        uses_max_liquidity: plan.uses_max_liquidity,
    })
}

/// Whether this bot's approvals have to go through its custodian.
///
/// True exactly when the signer cannot produce a transaction itself but the
/// panel can ask the custodian to. Today that is Fireblocks without
/// `raw_signing`; Turnkey and MPCVault sign digests directly and never need it.
/// The funding screen reads this to decide which button to offer, so it has to
/// agree with what [`approve_via_custody`] will actually accept.
pub fn custody_approvals_available(cfg: &Config) -> bool {
    matches!(cfg.signer.as_ref(), Some(SignerConfig::Fireblocks(c)) if !c.raw_signing)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shipped BSC preset, plus whatever `[signer]` the case needs.
    ///
    /// Built from the catalog rather than hand-rolled so the fixture cannot
    /// drift from the config an operator actually has on disk — a fixture that
    /// quietly stopped parsing like the real thing would test nothing.
    fn config_with(signer: &str) -> Config {
        let corridor =
            crate::setup::find_corridor("cngn-usdt-bsc").expect("the BSC preset is in the catalog");
        let toml = format!("{}\n{signer}", corridor.toml_template);
        Config::from_toml(&toml).expect("the preset plus a signer table parses")
    }

    const OPERATOR: &str = "0x6b8499a0e002d3ece3fdbb320a127c9f0f4d4fe5";

    const FIREBLOCKS: &str = r#"
[signer]
provider = "fireblocks"
vault_account_id = "0"
operator_address = "0x6b8499a0e002d3ece3fdbb320a127c9f0f4d4fe5"
"#;

    #[test]
    fn custody_is_offered_only_where_it_is_the_only_way() {
        // Fireblocks, typed messages only: the case this route exists for.
        assert!(custody_approvals_available(&config_with(FIREBLOCKS)));

        // Fireblocks with raw signing can sign its own transaction, so the
        // ordinary approval — which shares the bot's own nonce handling — wins.
        let raw = format!("{FIREBLOCKS}raw_signing = true\n");
        assert!(!custody_approvals_available(&config_with(&raw)));

        // A hot wallet has a key on disk and needs no custodian.
        assert!(!custody_approvals_available(&config_with("")));
    }

    const USDT: &str = "0x55d398326f99059fF775485246999027B3197955";
    const CNGN: &str = "0xa8AEA66B361a8d53e8865c62D142167Af28Af058";

    fn addr(hex: &str) -> Address {
        hex.parse().expect("a test address parses")
    }

    /// The route builds its own calldata and refuses a destination the bot's
    /// pools do not name. This is the check that keeps it from being a general
    /// "call any contract with the workspace key" endpoint.
    #[test]
    fn only_tokens_the_bot_trades_are_approvable() {
        let cfg = config_with(FIREBLOCKS);

        // Both sides of the corridor are approvable, and the spender is always
        // the config's Permit2 — never anything the caller chose.
        for token in [USDT, CNGN] {
            let plan = plan_for(&cfg, "bot-a", addr(token)).expect("a traded token is approvable");
            assert_eq!(
                plan.permit2,
                addr("0x000000000022D473030F116dDEE9F6B43aC78BA3")
            );
            assert_eq!(plan.owner, addr(OPERATOR), "the vault account's address");
        }

        // An address the config never names is refused before anything is
        // claimed, read or sent.
        let stranger = addr("0x000000000000000000000000000000000000dEaD");
        let err = plan_for(&cfg, "bot-a", stranger).expect_err("a stranger must be refused");
        assert!(
            format!("{err:?}").contains("is not a token"),
            "the refusal must say why: {err:?}"
        );

        // Including the Permit2 itself: it is the spender, never the target.
        let permit2 = addr("0x000000000022D473030F116dDEE9F6B43aC78BA3");
        assert!(plan_for(&cfg, "bot-a", permit2).is_err());
    }

    /// The two signer shapes this route declines, each for its own reason.
    #[test]
    fn a_signer_that_needs_no_custodian_is_turned_away() {
        let hot = plan_for(&config_with(""), "bot-a", addr(USDT));
        assert!(
            format!("{:?}", hot.expect_err("a hot wallet is refused"))
                .contains("not on a Fireblocks"),
            "a hot wallet signs its own transactions"
        );

        let raw = config_with(&format!("{FIREBLOCKS}raw_signing = true\n"));
        let err = plan_for(&raw, "bot-a", addr(USDT)).expect_err("raw signing is refused");
        assert!(
            format!("{err:?}").contains("raw signing is enabled")
                || format!("{err:?}").contains("has raw signing enabled"),
            "and it is refused because it does not need us: {err:?}"
        );
    }
}

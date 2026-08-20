// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! JSON frame types for the venue's maker WebSocket (`/v2/maker/stream`).
//!
//! Amounts are atomic-unit decimal strings, rates are RAY (1e27) decimal
//! strings, timestamps are ISO-8601 — all kept as strings here and converted
//! at the use site, so a frame round-trips byte-stable and a change in the
//! venue's precision can't silently truncate through a float.
//!
//! Both directions derive `Serialize` and `Deserialize`: the extra derive is
//! only used by tests, and it keeps the round-trip property checkable.

use serde::{Deserialize, Serialize};

// --- venue → maker ---

/// Every frame the venue sends. Internally tagged on `type`; an unknown tag
/// fails to parse and the read loop logs-and-ignores it, so the venue can add
/// frame types without breaking older makers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum VenueFrame {
    #[serde(rename = "challenge")]
    Challenge(ChallengeFrame),
    #[serde(rename = "sessionAccepted")]
    SessionAccepted(SessionAcceptedFrame),
    #[serde(rename = "quoteRequest")]
    QuoteRequest(QuoteRequestFrame),
    #[serde(rename = "quoteResult")]
    QuoteResult(QuoteResultFrame),
    #[serde(rename = "quoteExpired")]
    QuoteExpired(QuoteExpiredFrame),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeFrame {
    /// 32-byte hex challenge, echoed back verbatim in the session frame.
    pub challenge: String,
    pub expires_at: String,
    pub domain: SessionDomain,
}

/// The EIP-712 domain the venue asks the maker to sign under. The name is the
/// LIVE/TEST split; version is currently always "1".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDomain {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CorridorPairFrame {
    pub slug: String,
    pub chain_id: u64,
    pub collateral_token: String,
    pub debt_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAcceptedFrame {
    pub maker_id: String,
    pub signing_address: String,
    pub heartbeat_interval_ms: u64,
    pub heartbeat_timeout_ms: u64,
    /// Corridor slugs the venue routes to this maker.
    pub corridors: Vec<String>,
    /// Additive: tokens per slug so the bot can bind without `rfq_corridor`.
    #[serde(default)]
    pub corridor_pairs: Vec<CorridorPairFrame>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteRequestFrame {
    pub rfq_id: String,
    pub corridor_id: String,
    pub chain_id: u64,
    /// Token the TAKER sells — what the maker's signed order receives.
    pub sell_token: String,
    /// Token the taker buys — what the maker's signed order pays.
    pub buy_token: String,
    /// Exactly one of `sell_amount` / `buy_amount` is set.
    pub sell_amount: Option<String>,
    pub buy_amount: Option<String>,
    pub taker: String,
    /// Hard reply cutoff; the venue drops anything after it.
    pub reply_by: String,
    pub quote_ttl_ms: u64,
    pub max_expires_at: String,
    pub fee_bps: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResultFrame {
    pub rfq_id: String,
    /// `selected` | `lost_price` | `late` | `invalid` | `no_quote`. Kept as a
    /// string: results are informational and a new value must not break parsing.
    pub result: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteExpiredFrame {
    pub rfq_id: String,
}

// --- maker → venue ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MakerFrame {
    #[serde(rename = "session")]
    Session(SessionFrame),
    #[serde(rename = "levels")]
    Levels(LevelsFrame),
    #[serde(rename = "quoteResponse")]
    QuoteResponse(QuoteResponseFrame),
    #[serde(rename = "quoteReject")]
    QuoteReject(QuoteRejectFrame),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionFrame {
    pub maker_id: String,
    pub signing_address: String,
    /// The challenge hex from the venue, echoed verbatim.
    pub challenge: String,
    /// Unix milliseconds, a JSON number (not a string).
    pub issued_at: u64,
    /// 65-byte EIP-712 signature over the MakerSession struct, 0x-hex.
    pub signature: String,
    /// Which bot this is, so one funding wallet can run several — per chain,
    /// or several on one chain, even quoting the same corridor. The venue
    /// supersedes a session only when the same instance id reconnects, so this
    /// value must be stable across restarts and distinct per process.
    ///
    /// Outside the signature deliberately: it grants nothing (the venue still
    /// takes a session's corridors from its own registry), so it can only split
    /// this maker's own footprint. Omitted by older builds, which the venue
    /// then treats as one session per credential chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LevelsFrame {
    pub corridor_id: String,
    pub as_of: String,
    /// Maker buys collateral. Sizes are collateral atomic on BOTH sides.
    pub bids: Vec<Level>,
    /// Maker sells collateral.
    pub asks: Vec<Level>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Level {
    /// Collateral atomic units.
    pub size: String,
    /// Debt-per-collateral rate, decimal-normalized, RAY (1e27) scaled.
    pub rate_ray: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponseFrame {
    pub rfq_id: String,
    /// Gross the taker sends. Exact-input: echoes the request's sellAmount.
    pub sell_amount: String,
    /// What the maker pays — the signed order's input amount.
    pub buy_amount: String,
    /// Venue-fee projection: floor(output × feeBps / 10000). NOT an output of
    /// the signed order; the reactor's controller injects it at fill time.
    pub fee_amount: String,
    pub expires_at: String,
    /// `abi.encode(LimitOrder)`, 0x-hex.
    pub encoded_order: String,
    /// Permit2 witness signature, 0x-hex, 65 bytes.
    pub signature: String,
    /// The funding wallet (same address signs and funds).
    pub signer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteRejectFrame {
    pub rfq_id: String,
    pub reason: RejectReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    Inventory,
    ToxicTaker,
    StaleFeed,
    Size,
    Busy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_request_round_trips_the_documented_shape() {
        let json = r#"{
            "type": "quoteRequest",
            "rfqId": "rfq_123",
            "corridorId": "cngn-usdc",
            "chainId": 8453,
            "sellToken": "0x1111111111111111111111111111111111111111",
            "buyToken": "0x2222222222222222222222222222222222222222",
            "sellAmount": "1000000000",
            "buyAmount": null,
            "taker": "0x3333333333333333333333333333333333333333",
            "replyBy": "2026-08-05T10:00:00.750Z",
            "quoteTtlMs": 5000,
            "maxExpiresAt": "2026-08-05T10:02:00.000Z",
            "feeBps": 1
        }"#;
        let frame: VenueFrame = serde_json::from_str(json).unwrap();
        let VenueFrame::QuoteRequest(req) = &frame else {
            panic!("wrong variant: {frame:?}");
        };
        assert_eq!(req.rfq_id, "rfq_123");
        assert_eq!(req.sell_amount.as_deref(), Some("1000000000"));
        assert_eq!(req.buy_amount, None);
        assert_eq!(req.fee_bps, 1);
        assert_eq!(req.quote_ttl_ms, 5000);

        // Round trip: re-serialized fields keep their wire names and the tag.
        let out: serde_json::Value = serde_json::to_value(&frame).unwrap();
        assert_eq!(out["type"], "quoteRequest");
        assert_eq!(out["rfqId"], "rfq_123");
        assert_eq!(out["corridorId"], "cngn-usdc");
        assert_eq!(out["feeBps"], 1);
        let back: VenueFrame = serde_json::from_value(out).unwrap();
        let VenueFrame::QuoteRequest(req2) = back else {
            panic!("round trip changed the variant");
        };
        assert_eq!(req2.sell_amount, req.sell_amount);
        assert_eq!(req2.max_expires_at, req.max_expires_at);
    }

    #[test]
    fn quote_response_serializes_with_wire_field_names() {
        let frame = MakerFrame::QuoteResponse(QuoteResponseFrame {
            rfq_id: "rfq_123".into(),
            sell_amount: "1000000000".into(),
            buy_amount: "722000000".into(),
            fee_amount: "99990".into(),
            expires_at: "2026-08-05T10:00:05.000Z".into(),
            encoded_order: "0xdead".into(),
            signature: "0xbeef".into(),
            signer: "0x3333333333333333333333333333333333333333".into(),
        });
        let v: serde_json::Value = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["type"], "quoteResponse");
        assert_eq!(v["sellAmount"], "1000000000");
        assert_eq!(v["buyAmount"], "722000000");
        assert_eq!(v["feeAmount"], "99990");
        assert_eq!(v["encodedOrder"], "0xdead");
        assert_eq!(v["signer"], "0x3333333333333333333333333333333333333333");

        let back: MakerFrame = serde_json::from_value(v).unwrap();
        let MakerFrame::QuoteResponse(r) = back else {
            panic!("round trip changed the variant");
        };
        assert_eq!(r.fee_amount, "99990");
    }

    #[test]
    fn reject_reasons_use_snake_case_on_the_wire() {
        let frame = MakerFrame::QuoteReject(QuoteRejectFrame {
            rfq_id: "rfq_9".into(),
            reason: RejectReason::ToxicTaker,
        });
        let v: serde_json::Value = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["type"], "quoteReject");
        assert_eq!(v["reason"], "toxic_taker");
        for (reason, wire) in [
            (RejectReason::Inventory, "inventory"),
            (RejectReason::StaleFeed, "stale_feed"),
            (RejectReason::Size, "size"),
            (RejectReason::Busy, "busy"),
        ] {
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::Value::String(wire.into())
            );
        }
        let back: MakerFrame = serde_json::from_value(v).unwrap();
        let MakerFrame::QuoteReject(r) = back else {
            panic!("round trip changed the variant");
        };
        assert_eq!(r.reason, RejectReason::ToxicTaker);
    }

    #[test]
    fn session_frames_parse_the_documented_handshake() {
        let challenge: VenueFrame = serde_json::from_str(
            r#"{
                "type": "challenge",
                "challenge": "0x2222222222222222222222222222222222222222222222222222222222222222",
                "expiresAt": "2026-08-05T10:00:30.000Z",
                "domain": { "name": "Textile Maker Session (LIVE)", "version": "1" }
            }"#,
        )
        .unwrap();
        let VenueFrame::Challenge(c) = challenge else {
            panic!("wrong variant");
        };
        assert_eq!(c.domain.name, "Textile Maker Session (LIVE)");

        let accepted: VenueFrame = serde_json::from_str(
            r#"{
                "type": "sessionAccepted",
                "makerId": "mk_test",
                "signingAddress": "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
                "heartbeatIntervalMs": 15000,
                "heartbeatTimeoutMs": 45000,
                "corridors": ["cngn-usdc"]
            }"#,
        )
        .unwrap();
        let VenueFrame::SessionAccepted(a) = accepted else {
            panic!("wrong variant");
        };
        assert_eq!(a.heartbeat_timeout_ms, 45_000);
        assert_eq!(a.corridors, vec!["cngn-usdc"]);

        // issuedAt must serialize as a JSON number, not a string.
        let session = MakerFrame::Session(SessionFrame {
            maker_id: "mk_test".into(),
            signing_address: "0x7099…".into(),
            challenge: "0x2222…".into(),
            issued_at: 1_754_388_000_000,
            signature: "0xsig".into(),
            instance_id: Some("bsc-cngn".into()),
        });
        let v = serde_json::to_value(&session).unwrap();
        assert!(v["issuedAt"].is_u64());
        assert_eq!(v["issuedAt"], 1_754_388_000_000u64);
        assert_eq!(v["instanceId"], "bsc-cngn");
    }

    #[test]
    fn a_bot_without_an_instance_id_omits_the_field() {
        // The venue reads a missing instanceId as "one session per credential
        // chain", which is what an older build got. Sending `null` instead
        // would be a different thing to have to interpret.
        let session = MakerFrame::Session(SessionFrame {
            maker_id: "mk_test".into(),
            signing_address: "0x7099…".into(),
            challenge: "0x2222…".into(),
            issued_at: 1,
            signature: "0xsig".into(),
            instance_id: None,
        });
        let v = serde_json::to_value(&session).unwrap();
        assert!(v.get("instanceId").is_none());
    }

    #[test]
    fn an_unknown_frame_type_is_a_parse_error_not_a_panic() {
        let err = serde_json::from_str::<VenueFrame>(r#"{"type":"somethingNew","x":1}"#);
        assert!(err.is_err(), "unknown tags surface as errors to skip");
    }
}

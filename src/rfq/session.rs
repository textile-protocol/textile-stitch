// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Venue WebSocket session: connect with the maker credentials, answer the
//! EIP-712 challenge, and hand back an authenticated stream. Reconnection
//! policy lives in [`super::run`]; this module does exactly one attempt so
//! the backoff logic stays in one place.

use std::collections::BTreeSet;

use alloy_primitives::Address;
use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use crate::protocol::typed_data::maker_session_payload;
use crate::signer::DynSigner;

use super::wire::{MakerFrame, SessionAcceptedFrame, SessionFrame, VenueFrame};
use crate::time::unix_now_ms;

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long the whole connect + challenge + acceptance may take. Generous —
/// this is startup/reconnect, not the quote path.
const HANDSHAKE_TIMEOUT_SECS: u64 = 15;

/// Another session took this maker identity over on this chain.
const VENUE_SUPERSEDED_CODE: u16 = 4001;
/// The api task we reached does not hold the venue engine lease.
const VENUE_NOT_ENGINE_CODE: u16 = 4005;
/// The api task we were on is shutting down and has released the lease.
const VENUE_DRAINING_CODE: u16 = 4006;
/// Our seat list went stale: a corridor was listed (or left Book mode) after we
/// authenticated. Reconnecting is what picks it up — the venue hands the
/// corridor pairs down in SessionAccepted and nothing refreshes them in place.
const VENUE_SEATS_CHANGED_CODE: u16 = 4007;

/// The venue told us to move, and nothing is wrong with this maker.
///
/// Worth its own type because the reconnect policy for it is the opposite of
/// the policy for a failure: backing off is exactly wrong. These closes happen
/// during a deploy, when a warm replacement is already accepting sockets, so
/// every second of backoff is a second of a corridor reporting no makers for
/// no reason. Everything else still backs off — a venue that is genuinely down
/// must not be hammered.
#[derive(Debug, thiserror::Error)]
#[error("venue handover ({code}): {reason}")]
pub struct VenueHandover {
    pub code: u16,
    pub reason: String,
}

/// Another socket claimed this maker identity, so the venue closed ours.
///
/// Once, this is the standby-failover handoff working as intended. Over and
/// over it means two processes are quoting as one identity on one chain, and
/// neither ever stays connected long enough to be routed — the venue counts the
/// maker as online while every corridor it should be quoting sits dark. Typed
/// so the reconnect loop can say that out loud instead of logging its
/// hundredth "session ended" line.
#[derive(Debug, thiserror::Error)]
#[error("venue superseded this session ({code}): {reason}")]
pub struct VenueSuperseded {
    pub code: u16,
    pub reason: String,
}

/// The venue answered the WebSocket upgrade with 429 Too Many Requests.
///
/// The venue rate-limits upgrades per client address, and every bot on one host
/// shares that address. Typed so the reconnect loop waits for the `Retry-After`
/// the venue sent instead of retrying into the same window and pushing the
/// whole host's count further over.
#[derive(Debug, thiserror::Error)]
#[error("venue rate-limited this connection (429), retry after {retry_after:?}")]
pub struct VenueRateLimited {
    pub retry_after: Option<std::time::Duration>,
}

/// `Retry-After` in its delay-seconds form. The HTTP-date form is not something
/// the venue sends, and guessing a clock offset for it would be worse than
/// falling back to the normal backoff.
fn retry_after_secs(value: &str) -> Option<std::time::Duration> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(std::time::Duration::from_secs)
}

/// Turn a failed upgrade into [`VenueRateLimited`] when it was a 429, and leave
/// everything else as it was.
fn classify_connect_error(err: tokio_tungstenite::tungstenite::Error) -> anyhow::Error {
    use tokio_tungstenite::tungstenite::http::StatusCode;
    use tokio_tungstenite::tungstenite::Error as WsError;
    match err {
        WsError::Http(response) if response.status() == StatusCode::TOO_MANY_REQUESTS => {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(retry_after_secs);
            anyhow::Error::new(VenueRateLimited { retry_after })
        }
        other => anyhow::Error::new(other),
    }
}

/// How long the venue asked us to wait, if this failure was a rate limit.
/// `Some(None)` is a 429 with no usable `Retry-After`.
pub fn rate_limited(err: &anyhow::Error) -> Option<Option<std::time::Duration>> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<VenueRateLimited>())
        .map(|limited| limited.retry_after)
}

/// Does this close frame mean "reconnect now" rather than "go away"?
///
/// Two shapes, same policy: the task is wrong (4005/4006), or our seat list is
/// (4007). In all three the socket and the credential are fine and the venue
/// wants us straight back, so backing off is exactly wrong.
pub fn handover_close(code: u16) -> bool {
    code == VENUE_NOT_ENGINE_CODE || code == VENUE_DRAINING_CODE || code == VENUE_SEATS_CHANGED_CODE
}

/// Was this failure another session taking our identity?
pub fn is_superseded(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<VenueSuperseded>().is_some())
}

/// Classify a close frame, so both the handshake and the session loop report a
/// handover the same way.
pub fn close_error(
    reason: &Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>,
) -> anyhow::Error {
    let code = reason.as_ref().map(|f| u16::from(f.code)).unwrap_or(0);
    let text = reason
        .as_ref()
        .map(|f| f.reason.to_string())
        .unwrap_or_default();
    if handover_close(code) {
        return anyhow::Error::new(VenueHandover { code, reason: text });
    }
    if code == VENUE_SUPERSEDED_CODE {
        return anyhow::Error::new(VenueSuperseded { code, reason: text });
    }
    anyhow::anyhow!("venue closed the session: {code} {text}")
}

/// Was this failure a handover instruction anywhere in its chain?
pub fn is_handover(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<VenueHandover>().is_some())
}

pub struct AuthedSession {
    pub stream: WsStream,
    pub accepted: SessionAcceptedFrame,
}

/// One connection attempt: dial, authenticate, return the accepted session.
pub async fn connect_and_auth(
    url: &str,
    api_key: &str,
    maker_id: &str,
    instance_id: Option<&str>,
    allowed_takers: Option<&BTreeSet<Address>>,
    signer: &DynSigner,
) -> anyhow::Result<AuthedSession> {
    tokio::time::timeout(
        std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        handshake(url, api_key, maker_id, instance_id, allowed_takers, signer),
    )
    .await
    .context("venue handshake timed out")?
}

async fn handshake(
    url: &str,
    api_key: &str,
    maker_id: &str,
    instance_id: Option<&str>,
    allowed_takers: Option<&BTreeSet<Address>>,
    signer: &DynSigner,
) -> anyhow::Result<AuthedSession> {
    let mut request = url
        .into_client_request()
        .context("building the venue WebSocket request")?;
    let headers = request.headers_mut();
    headers.insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {api_key}"))
            .context("maker api key is not a valid header value")?,
    );
    headers.insert(
        "X-Textile-Maker-Id",
        HeaderValue::from_str(maker_id).context("maker id is not a valid header value")?,
    );

    let (mut stream, _) = connect_async(request)
        .await
        .map_err(classify_connect_error)
        .context("connecting to venue")?;

    // The venue speaks first: wait for the challenge, ignoring anything else.
    let challenge = loop {
        match next_frame(&mut stream).await? {
            VenueFrame::Challenge(c) => break c,
            other => debug!(frame = ?other, "ignoring pre-challenge frame"),
        }
    };

    let challenge_hash = challenge
        .challenge
        .parse()
        .context("venue challenge is not 32-byte hex")?;
    let issued_at = unix_now_ms();
    let payload = maker_session_payload(
        &challenge.domain.name,
        maker_id,
        signer.address(),
        challenge_hash,
        issued_at,
    );
    let signature = signer
        .sign_typed(&payload)
        .await
        .context("signing the maker session challenge")?;

    let session = MakerFrame::Session(SessionFrame {
        maker_id: maker_id.to_string(),
        signing_address: signer.address().to_string(),
        challenge: challenge.challenge.clone(),
        issued_at,
        signature: alloy_primitives::hex::encode_prefixed(signature),
        instance_id: instance_id.map(str::to_string),
        allowed_takers: allowed_takers_wire(allowed_takers),
    });
    stream
        .send(Message::text(serde_json::to_string(&session)?))
        .await
        .context("sending the session frame")?;

    let accepted = loop {
        match next_frame(&mut stream).await? {
            VenueFrame::SessionAccepted(a) => break a,
            other => debug!(frame = ?other, "ignoring pre-acceptance frame"),
        }
    };
    info!(
        maker_id,
        instance_id = ?instance_id,
        allowed_takers = allowed_takers.map(BTreeSet::len),
        signing_address = %signer.address(),
        corridors = ?accepted.corridors,
        domain = %challenge.domain.name,
        "RFQ session accepted"
    );
    Ok(AuthedSession { stream, accepted })
}

/// The session frame's `allowedTakers`: lowercased 0x hex in address order,
/// or `None` so the field is left out when the bot quotes every taker.
fn allowed_takers_wire(allowed: Option<&BTreeSet<Address>>) -> Option<Vec<String>> {
    allowed.map(|set| set.iter().map(|a| format!("{a:#x}")).collect())
}

/// Read venue frames until one parses, answering protocol pings along the way.
/// A close or transport error is an error — the caller reconnects.
async fn next_frame(stream: &mut WsStream) -> anyhow::Result<VenueFrame> {
    loop {
        let msg = stream
            .next()
            .await
            .context("venue closed the stream during the handshake")??;
        match msg {
            Message::Text(text) => match serde_json::from_str::<VenueFrame>(text.as_str()) {
                Ok(frame) => return Ok(frame),
                Err(e) => debug!(error = %e, raw = %text, "unparseable venue frame; skipping"),
            },
            Message::Ping(payload) => stream.send(Message::Pong(payload)).await?,
            Message::Close(reason) => {
                // A handover close here is the normal overlapping-deploy path:
                // the ALB handed us a task that isn't the engine. Not a warning.
                if handover_close(reason.as_ref().map(|f| u16::from(f.code)).unwrap_or(0)) {
                    debug!(?reason, "venue redirected us during the handshake");
                } else {
                    warn!(?reason, "venue closed the connection during the handshake");
                }
                return Err(close_error(&reason));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_takers_go_on_the_wire_lowercased_or_not_at_all() {
        assert_eq!(allowed_takers_wire(None), None);
        let set: BTreeSet<Address> = [
            "0x00000000000000000000000000000000000000CD",
            "0xAbCdEf0000000000000000000000000000000001",
        ]
        .iter()
        .map(|a| a.parse().unwrap())
        .collect();
        assert_eq!(
            allowed_takers_wire(Some(&set)),
            Some(vec![
                "0x00000000000000000000000000000000000000cd".to_string(),
                "0xabcdef0000000000000000000000000000000001".to_string(),
            ])
        );
    }

    fn http_error(status: u16, retry_after: Option<&str>) -> tokio_tungstenite::tungstenite::Error {
        let mut builder = tokio_tungstenite::tungstenite::http::Response::builder().status(status);
        if let Some(value) = retry_after {
            builder = builder.header("Retry-After", value);
        }
        tokio_tungstenite::tungstenite::Error::Http(builder.body(None).unwrap())
    }

    #[test]
    fn a_429_carries_the_venues_retry_after() {
        let err =
            classify_connect_error(http_error(429, Some("45"))).context("connecting to venue");
        assert_eq!(
            rate_limited(&err),
            Some(Some(std::time::Duration::from_secs(45)))
        );
    }

    #[test]
    fn a_429_without_a_usable_retry_after_is_still_a_rate_limit() {
        let missing = classify_connect_error(http_error(429, None));
        assert_eq!(rate_limited(&missing), Some(None));
        let dated = classify_connect_error(http_error(429, Some("Fri, 20 Nov 2026 13:46:37 GMT")));
        assert_eq!(rate_limited(&dated), Some(None));
    }

    /// Over a real socket: the venue's throttle answers the upgrade with a
    /// plain HTTP 429 before any WebSocket exists.
    #[tokio::test]
    async fn a_venue_429_on_the_upgrade_is_read_as_a_rate_limit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(
                    b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 37\r\n\
                      Content-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let err = connect_async(format!("ws://127.0.0.1:{port}/v2/maker/stream"))
            .await
            .map(|_| ())
            .map_err(classify_connect_error)
            .context("connecting to venue")
            .unwrap_err();
        assert_eq!(
            rate_limited(&err),
            Some(Some(std::time::Duration::from_secs(37)))
        );
    }

    #[test]
    fn other_upgrade_failures_are_not_a_rate_limit() {
        let err = classify_connect_error(http_error(503, Some("5")));
        assert_eq!(rate_limited(&err), None);
        assert!(format!("{err:#}").contains("503"));
    }
}

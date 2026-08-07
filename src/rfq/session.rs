// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Venue WebSocket session: connect with the maker credentials, answer the
//! EIP-712 challenge, and hand back an authenticated stream. Reconnection
//! policy lives in [`super::run`]; this module does exactly one attempt so
//! the backoff logic stays in one place.

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use crate::eip712::maker_session_digest;
use crate::signer::DynSigner;

use super::time::unix_ms_now;
use super::wire::{MakerFrame, SessionAcceptedFrame, SessionFrame, VenueFrame};

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long the whole connect + challenge + acceptance may take. Generous —
/// this is startup/reconnect, not the quote path.
const HANDSHAKE_TIMEOUT_SECS: u64 = 15;

pub struct AuthedSession {
    pub stream: WsStream,
    pub accepted: SessionAcceptedFrame,
}

/// One connection attempt: dial, authenticate, return the accepted session.
pub async fn connect_and_auth(
    url: &str,
    api_key: &str,
    maker_id: &str,
    signer: &DynSigner,
) -> anyhow::Result<AuthedSession> {
    tokio::time::timeout(
        std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        handshake(url, api_key, maker_id, signer),
    )
    .await
    .context("venue handshake timed out")?
}

async fn handshake(
    url: &str,
    api_key: &str,
    maker_id: &str,
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
    let issued_at = unix_ms_now();
    let digest = maker_session_digest(
        &challenge.domain.name,
        maker_id,
        signer.address(),
        challenge_hash,
        issued_at,
    );
    let signature = signer
        .sign_digest(digest)
        .await
        .context("signing the maker session challenge")?;

    let session = MakerFrame::Session(SessionFrame {
        maker_id: maker_id.to_string(),
        signing_address: signer.address().to_string(),
        challenge: challenge.challenge.clone(),
        issued_at,
        signature: alloy_primitives::hex::encode_prefixed(signature),
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
        signing_address = %signer.address(),
        corridors = ?accepted.corridors,
        domain = %challenge.domain.name,
        "RFQ session accepted"
    );
    Ok(AuthedSession { stream, accepted })
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
                warn!(?reason, "venue closed the connection during the handshake");
                anyhow::bail!("venue closed during handshake: {reason:?}");
            }
            _ => {}
        }
    }
}

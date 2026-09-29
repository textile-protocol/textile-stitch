// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Shared HTTP client settings for all external Stitch dependencies.

use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Why a redirect failed, as it shows up in the fetch error.
const REDIRECT_REFUSED: &str = "redirects are refused: Stitch only talks to the configured URL";

/// The client for every configured origin: price feeds, the indexer, the
/// subgraph, the JSON-RPC node and the corridor list.
///
/// Redirects are refused. The URL checks at config load (https, or http only
/// on a private host) only see the first hop, so following a 3xx would let an
/// origin, or anyone on the path of a plaintext second hop, pick where a price
/// actually comes from. None of these endpoints needs a redirect. Not
/// `https_only(true)`: that would also reject the loopback http feeds
/// `assert_feed_url` allows on purpose.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            attempt.error(REDIRECT_REFUSED)
        }))
        .build()
        .expect("Stitch HTTP client configuration is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::feed::{HttpFeed, PriceFeed};

    #[test]
    fn builds_a_bounded_http_client() {
        let _ = http_client();
    }

    /// A server that answers every request with `head` and a price body, and
    /// counts how many requests it served.
    async fn serve(head: String) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let head = head.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let body = r#"{"price":1.0,"timestamp":1}"#;
                    let resp = format!(
                        "{head}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), hits)
    }

    #[tokio::test]
    async fn a_feed_that_redirects_is_an_error_and_the_target_is_never_contacted() {
        use std::sync::atomic::Ordering;
        let (target, target_hits) = serve("HTTP/1.1 200 OK".into()).await;
        let (feed, _) = serve(format!(
            "HTTP/1.1 302 Found\r\nlocation: {target}/latest/meta-data/"
        ))
        .await;

        let err = HttpFeed::new(format!("{feed}/price"))
            .fetch()
            .await
            .expect_err("a redirected feed must not produce a price");

        assert!(
            format!("{err:#}").contains(REDIRECT_REFUSED),
            "names the refused redirect: {err:#}"
        );
        assert_eq!(target_hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_feed_on_the_configured_url_still_prices() {
        let (feed, _) = serve("HTTP/1.1 200 OK".into()).await;
        let quote = HttpFeed::new(format!("{feed}/price"))
            .fetch()
            .await
            .unwrap();
        assert_eq!(quote.price, 1.0);
    }
}

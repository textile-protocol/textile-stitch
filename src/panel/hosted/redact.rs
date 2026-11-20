// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! URLs as a hosted panel shows them.
//!
//! RPC and feed URLs often carry a provider key, and there's no telling where:
//! in the userinfo, as a path segment of any shape (Alchemy `/v2/<key>`, a
//! digit-free token, a key split over two segments), in the query or in the
//! fragment. A hosted panel is run for a client by Textile's operators, who
//! need to see which provider a bot talks to but not its key. So [`url`] keeps
//! only `scheme://host[:port]` and replaces everything after it with one
//! `/REDACTED` ([`MASK`]), without guessing which part is the key. A URL that
//! has nothing after its host comes back unchanged. The full profile shows URLs
//! as they are.
//!
//! "Every URL it returns" includes URLs inside other text: an RPC timeout in a
//! funding or allowance read, a feed error, a bot's log line. So a hosted panel
//! runs every JSON response through [`mask_responses`], which masks each URL
//! found in any string ([`urls_in_text`]), and its log tail masks each line the
//! same way.
//!
//! A masked URL is for display only. A save that sends one back unchanged
//! keeps the stored value ([`is_echo`]); anything else carrying the mask is
//! refused, so the mask can never be written into a config.

/// What a URL's userinfo, path, query and fragment become, as one path segment.
pub const MASK: &str = "REDACTED";

/// `raw` as `scheme://host[:port]`, plus `/REDACTED` when it had a userinfo, a
/// path other than `/`, a query or a fragment. A URL with only a host comes
/// back as the parser normalizes it (`https://x.example/`). Empty stays empty.
/// A value that doesn't parse as a URL with a host is masked whole, since
/// there's no telling which part of it is the key.
pub fn url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let Ok(parsed) = url::Url::parse(trimmed) else {
        return MASK.to_string();
    };
    let Some(host) = parsed.host_str().filter(|_| !parsed.cannot_be_a_base()) else {
        return MASK.to_string();
    };
    if !has_more_than_origin(&parsed) {
        return parsed.to_string();
    }
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    format!("{}://{host}{port}/{MASK}", parsed.scheme())
}

/// Whether `parsed` carries anything [`url`] has to remove. An empty query or
/// fragment (`https://x/?`) holds nothing.
fn has_more_than_origin(parsed: &url::Url) -> bool {
    !parsed.username().is_empty()
        || parsed.password().is_some()
        || !matches!(parsed.path(), "" | "/")
        || parsed.query().is_some_and(|q| !q.is_empty())
        || parsed.fragment().is_some_and(|f| !f.is_empty())
}

/// `text` with every `scheme://...` run in it replaced by [`url`] of it. A run
/// ends at whitespace, a quote or a bracket; trailing `.`, `,`, `;` and `:`
/// stay outside it, as in prose.
pub fn urls_in_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let start = rest[..at]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')))
            .map_or(0, |i| i + 1);
        let tail = &rest[at + 3..];
        let len = tail
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | ')' | ']' | '}')
            })
            .unwrap_or(tail.len());
        let run = tail[..len].trim_end_matches(['.', ',', ';', ':']);
        let end = at + 3 + run.len();
        if start == at {
            // No scheme in front: not a URL, keep it and move on.
            out.push_str(&rest[..at + 3]);
            rest = &rest[at + 3..];
            continue;
        }
        out.push_str(&rest[..start]);
        out.push_str(&url(&rest[start..end]));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Every string in `value`, with [`urls_in_text`] applied.
pub fn urls_in_json(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::String(s) if s.contains("://") => *s = urls_in_text(s),
        Value::Array(items) => items.iter_mut().for_each(urls_in_json),
        Value::Object(map) => map.values_mut().for_each(urls_in_json),
        _ => {}
    }
}

/// Largest JSON response the hosted layer rewrites. Panel responses are far
/// smaller; anything over this is refused rather than sent unmasked.
const MAX_JSON_RESPONSE: usize = 64 * 1024 * 1024;

/// Hosted route layer: every JSON response, errors included, leaves with its
/// URLs masked. Other bodies (the log tail's event stream, which masks its own
/// lines, and static files) pass through.
pub async fn mask_responses(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    let response = next.run(req).await;
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if !is_json {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let refused = || {
        crate::panel::http::ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "this hosted panel couldn't check a response for credentials, so it wasn't sent",
        )
        .into_response()
    };
    let Ok(bytes) = axum::body::to_bytes(body, MAX_JSON_RESPONSE).await else {
        return refused();
    };
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return refused();
    };
    urls_in_json(&mut value);
    let Ok(masked) = serde_json::to_vec(&value) else {
        return refused();
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    axum::response::Response::from_parts(parts, axum::body::Body::from(masked))
}

/// Whether `sent` is what [`url`] showed for `stored`, i.e. the client sent the
/// masked value back without editing it.
pub fn is_echo(sent: &str, stored: &str) -> bool {
    let sent = sent.trim();
    sent == url(stored) || sent == stored.trim()
}

/// Whether `text` carries the mask, so it must never be written.
pub fn carries_mask(text: &str) -> bool {
    text.contains(MASK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_after_the_host_is_masked_whatever_its_shape() {
        let cases = [
            // Provider keys in the path, with and without digits.
            (
                "https://eth-mainnet.g.alchemy.com/v2/AbCdEfGh1234567890_-xyzKLMN",
                "https://eth-mainnet.g.alchemy.com/REDACTED",
            ),
            (
                "https://mainnet.infura.io/v3/9aa3d95b3bc440fa88ea12eaa4456161",
                "https://mainnet.infura.io/REDACTED",
            ),
            (
                "https://rpc.example.com/abcdefghijklmnopqrstuvwxyzABCDEF",
                "https://rpc.example.com/REDACTED",
            ),
            (
                "https://rpc.example.com/v1/short-secret/bsc/",
                "https://rpc.example.com/REDACTED",
            ),
            (
                "wss://bsc-mainnet.core.chainstack.com/ws/3c6e0b8a9c15224a8228b9a98ca1531d",
                "wss://bsc-mainnet.core.chainstack.com/REDACTED",
            ),
            // Query, fragment and userinfo, whatever the parameter is called.
            (
                "https://feed.example/price?pair=zar&apikey=sk_live_123",
                "https://feed.example/REDACTED",
            ),
            (
                "https://feed.example/?pair=zar&x=nodigitsatall",
                "https://feed.example/REDACTED",
            ),
            (
                "https://feed.example/#s3cret",
                "https://feed.example/REDACTED",
            ),
            (
                "https://user:hunter2@rpc.example.com/",
                "https://rpc.example.com/REDACTED",
            ),
            (
                "https://onlyuser@rpc.example.com",
                "https://rpc.example.com/REDACTED",
            ),
        ];
        for (raw, shown) in cases {
            assert_eq!(url(raw), shown, "{raw}");
        }
    }

    #[test]
    fn the_port_is_kept() {
        assert_eq!(
            url("http://127.0.0.1:8545/v3/key"),
            "http://127.0.0.1:8545/REDACTED"
        );
        assert_eq!(url("wss://[::1]:8546/ws?t=x"), "wss://[::1]:8546/REDACTED");
        // A default port is the parser's to drop, as it would anyway.
        assert_eq!(url("https://h.example:443/k"), "https://h.example/REDACTED");
    }

    #[test]
    fn a_url_with_only_a_host_is_unchanged_and_has_no_marker() {
        for plain in [
            "https://bsc-dataseed.binance.org/",
            "http://127.0.0.1:8545/",
            "wss://api.textilecredit.com/",
        ] {
            assert_eq!(url(plain), plain);
            assert!(!carries_mask(&url(plain)), "{plain}");
        }
        assert_eq!(url("https://x.example"), "https://x.example/");
        assert_eq!(url("https://x.example/?"), "https://x.example/?");
        assert_eq!(url(""), "");
    }

    #[test]
    fn urls_inside_text_are_masked_and_the_text_around_them_kept() {
        let text = "the RPC at https://mainnet.infura.io/v3/9aa3d95b3bc440fa88ea12eaa4456161 didn't \
                    answer; feed (https://feed.example/p?apikey=sk_live_1). Plain https://x.example/ ok";
        assert_eq!(
            urls_in_text(text),
            "the RPC at https://mainnet.infura.io/REDACTED didn't answer; feed \
             (https://feed.example/REDACTED). Plain https://x.example/ ok"
        );
        assert_eq!(
            urls_in_text("no url here :// at all"),
            "no url here :// at all"
        );
        let mut v = serde_json::json!({"a": ["wss://u:p@h.example/x"], "b": {"c": 1}});
        urls_in_json(&mut v);
        assert_eq!(v["a"][0], "wss://h.example/REDACTED");
        // Masking twice changes nothing.
        let once = urls_in_text(text);
        assert_eq!(urls_in_text(&once), once);
    }

    #[test]
    fn something_that_is_not_a_url_is_masked_whole() {
        assert_eq!(url("not a url with sk_live_123"), MASK);
        assert_eq!(url("mailto:someone@example.com"), MASK);
        assert_eq!(url("file:///etc/stitch/key"), MASK);
    }

    #[test]
    fn an_unedited_masked_value_is_an_echo_and_anything_else_with_the_marker_is_not() {
        let stored = "https://rpc.example.com/abcdefghijklmnopqrstuvwxyzABCDEF";
        let shown = url(stored);
        assert!(is_echo(&shown, stored));
        assert!(is_echo(&format!("  {shown} "), stored));
        assert!(is_echo(stored, stored));
        assert!(carries_mask(&shown));
        assert!(!carries_mask(stored));
        // Edited, moved to another host, or another URL's mask: refused on write.
        for sent in [
            "https://rpc.example.com/REDACTED/x",
            "https://other.example.com/REDACTED",
            "https://rpc.example.com:8545/REDACTED",
            "https://REDACTED@rpc.example.com/",
        ] {
            assert!(!is_echo(sent, stored), "{sent}");
            assert!(carries_mask(sent), "{sent}");
        }
        // A host-only URL shows as itself, so sending it back is an echo too.
        assert!(is_echo(
            &url("http://127.0.0.1:8545/"),
            "http://127.0.0.1:8545/"
        ));
    }
}

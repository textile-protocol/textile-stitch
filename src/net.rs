// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Shared HTTP client settings for all external Stitch dependencies.

use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("Stitch HTTP client configuration is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_bounded_http_client() {
        let _ = http_client();
    }
}

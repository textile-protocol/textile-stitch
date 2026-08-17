// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Block-explorer URLs for the chains the corridor catalog ships.
//!
//! The panel uses this to link a hot-wallet operator address. A corridor on a
//! chain that isn't in this map gets no link — never a guessed host.

/// Browser origin for a chain's explorer, with no trailing slash.
pub fn explorer_base_url(chain_id: u64) -> Option<&'static str> {
    Some(match chain_id {
        1 => "https://etherscan.io",
        56 => "https://bscscan.com",
        97 => "https://testnet.bscscan.com",
        8453 => "https://basescan.org",
        42220 => "https://celoscan.io",
        4663 => "https://robinhoodchain.blockscout.com",
        _ => return None,
    })
}

/// Address page on that chain's explorer, or `None` when the chain is unknown
/// or the address isn't a 20-byte hex account.
pub fn address_explorer_url(chain_id: u64, address: &str) -> Option<String> {
    let base = explorer_base_url(chain_id)?;
    let address = address.trim();
    if !is_account_address(address) {
        return None;
    }
    Some(format!("{base}/address/{address}"))
}

fn is_account_address(address: &str) -> bool {
    let Some(hex) = address
        .strip_prefix("0x")
        .or_else(|| address.strip_prefix("0X"))
    else {
        return false;
    };
    hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::catalog;

    const ADDR: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    #[test]
    fn known_chains_get_the_right_host() {
        assert_eq!(
            address_explorer_url(56, ADDR).as_deref(),
            Some("https://bscscan.com/address/0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")
        );
        assert_eq!(
            address_explorer_url(42220, ADDR).as_deref(),
            Some("https://celoscan.io/address/0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")
        );
        assert_eq!(
            address_explorer_url(8453, ADDR).as_deref(),
            Some("https://basescan.org/address/0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")
        );
        assert_eq!(
            address_explorer_url(1, ADDR).as_deref(),
            Some("https://etherscan.io/address/0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")
        );
        assert_eq!(
            address_explorer_url(4663, ADDR).as_deref(),
            Some(
                "https://robinhoodchain.blockscout.com/address/0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
            )
        );
        assert_eq!(
            address_explorer_url(97, ADDR).as_deref(),
            Some("https://testnet.bscscan.com/address/0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")
        );
    }

    #[test]
    fn unknown_chain_or_junk_address_is_no_link() {
        assert_eq!(address_explorer_url(31337, ADDR), None);
        assert_eq!(address_explorer_url(56, ""), None);
        assert_eq!(address_explorer_url(56, "not-an-address"), None);
        assert_eq!(address_explorer_url(56, "0xdead"), None);
    }

    #[test]
    fn every_catalog_chain_has_an_explorer() {
        for corridor in catalog() {
            assert!(
                explorer_base_url(corridor.chain_id).is_some(),
                "corridor {} is on chain {} with no explorer host — add it in explorer.rs",
                corridor.id,
                corridor.chain_id
            );
        }
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
const STARTUP_BANNER: &str = r#" ____ _____ ___ _____ ____ _   _
/ ___|_   _|_ _|_   _/ ___| | | |
\___ \ | |  | |  | || |   | |_| |
 ___) || |  | |  | || |___|  _  |
|____/ |_| |___| |_| \____|_| |_|

A filler bot for Textile FX
"#;

pub fn print_startup_banner() {
    println!("{STARTUP_BANNER}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_banner_names_the_bot() {
        assert!(STARTUP_BANNER.contains("____ _____ ___ _____ ____"));
        assert!(STARTUP_BANNER.contains("A filler bot for Textile FX"));
    }
}

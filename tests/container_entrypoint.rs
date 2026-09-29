// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The container entrypoint files every inline secret to a 0600 file and keeps
//! it out of the environment it execs the bot with.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const ENTRYPOINT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/deploy/container-entrypoint.sh"
);

/// (inline env var, file env var, file name) for every secret the signer
/// backends and the RFQ client read. Keep in step with the entrypoint.
const SECRETS: &[(&str, &str, &str)] = &[
    (
        "STITCH_PRIVATE_KEY",
        "STITCH_PRIVATE_KEY_FILE",
        "stitch.key",
    ),
    (
        "STITCH_RFQ_API_KEY",
        "STITCH_RFQ_API_KEY_FILE",
        "rfq-api.key",
    ),
    (
        "TURNKEY_API_PRIVATE_KEY",
        "TURNKEY_API_PRIVATE_KEY_FILE",
        "turnkey-api.key",
    ),
    (
        "MPCVAULT_API_TOKEN",
        "MPCVAULT_API_TOKEN_FILE",
        "mpcvault-api.token",
    ),
    (
        "FIREBLOCKS_API_PRIVATE_KEY",
        "FIREBLOCKS_API_PRIVATE_KEY_FILE",
        "fireblocks-api.key",
    ),
];

fn runtime_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("stitch-entrypoint-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn secret_value(var: &str) -> String {
    // Multi-line, like the Fireblocks RSA PEM.
    format!("-----BEGIN {var}-----\nsecret-{var}\n-----END {var}-----")
}

/// Run the entrypoint with every inline secret set and return the environment
/// it execs its command with.
fn exec_env(dir: &Path) -> String {
    let mut cmd = Command::new("sh");
    cmd.arg(ENTRYPOINT)
        .arg("env")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("STITCH_RUNTIME_DIR", dir);
    for (var, _, _) in SECRETS {
        cmd.env(var, secret_value(var));
    }
    let out = cmd.output().expect("run the entrypoint");
    assert!(
        out.status.success(),
        "entrypoint failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn every_inline_secret_is_filed_and_dropped_from_the_exec_environment() {
    let dir = runtime_dir("filed");
    let env = exec_env(&dir);

    for (var, file_var, file) in SECRETS {
        let path = dir.join(file);
        assert!(
            !env.lines().any(|l| l.starts_with(&format!("{var}="))),
            "{var} survived into the exec'd environment"
        );
        assert!(
            !env.contains(&format!("secret-{var}")),
            "{var}'s value survived into the exec'd environment"
        );
        assert!(
            env.lines()
                .any(|l| l == format!("{file_var}={}", path.display())),
            "{file_var} not exported"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim_end(),
            secret_value(var)
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{file} is {mode:o}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_mounted_file_is_left_alone_when_no_inline_secret_is_set() {
    let dir = runtime_dir("mounted");
    let out = Command::new("sh")
        .arg(ENTRYPOINT)
        .arg("env")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("STITCH_RUNTIME_DIR", &dir)
        .env("FIREBLOCKS_API_PRIVATE_KEY_FILE", "/mnt/fireblocks.pem")
        .output()
        .expect("run the entrypoint");
    assert!(out.status.success());
    let env = String::from_utf8(out.stdout).unwrap();
    assert!(env
        .lines()
        .any(|l| l == "FIREBLOCKS_API_PRIVATE_KEY_FILE=/mnt/fireblocks.pem"));
    assert!(!dir.join("fireblocks-api.key").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! A UI-created password lives in the mounted fleet directory, not the container
//! environment. Its presence disables legacy identity login on every startup.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{ensure, Context, Result};
use argon2::password_hash::PasswordHash;
use serde::{Deserialize, Serialize};

pub const PASSWORD_FILE: &str = ".stitch-panel-password.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPassword {
    version: u8,
    password_hash: String,
}

/// Missing is the only fallback to environment auth. Unreadable or corrupt state
/// must stop startup, rather than silently restoring the old header bypass.
pub fn load(root: &Path) -> Result<Option<String>> {
    let path = root.join(PASSWORD_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(
            metadata.is_file(),
            "saved panel password must be a regular file"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("reading the saved panel password"),
    }
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) => return Err(e).context("reading the saved panel password"),
    };
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 4096,
        "saved panel password file is too large"
    );
    let stored: StoredPassword = serde_json::from_slice(&bytes)
        .context("the saved panel password file is invalid; refusing identity login")?;
    ensure!(
        stored.version == 1,
        "unsupported saved panel password version"
    );
    validate_hash(&stored.password_hash)?;
    Ok(Some(stored.password_hash))
}

fn validate_hash(hash: &str) -> Result<()> {
    let parsed = PasswordHash::new(hash)
        .map_err(|_| anyhow::anyhow!("invalid saved panel password hash"))?;
    ensure!(
        parsed.algorithm.as_str() == "argon2id" && parsed.salt.is_some() && parsed.hash.is_some(),
        "invalid saved panel password hash"
    );
    Ok(())
}

/// Publish a complete 0600 file without replacing any existing password. The
/// exclusive hard link is the commit point, including across concurrent writers.
pub fn save(root: &Path, hash: &str) -> Result<()> {
    validate_hash(hash)?;
    let path = root.join(PASSWORD_FILE);
    let suffix = rand::random::<u128>();
    let temp = root.join(format!("{PASSWORD_FILE}.{suffix:032x}.tmp"));
    #[cfg(unix)]
    let directory = fs::File::open(root).context("opening the panel password directory")?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .context("creating the panel password file")?;
    let result = (|| {
        let stored = StoredPassword {
            version: 1,
            password_hash: hash.into(),
        };
        file.write_all(&serde_json::to_vec(&stored)?)?;
        file.sync_all()?;
        fs::hard_link(&temp, &path)
            .context("saving the panel password; an existing password cannot be replaced")?;
        // The password is published now. A directory-sync failure must not leave
        // this process accepting legacy login while the next boot uses a password.
        #[cfg(unix)]
        if let Err(error) = directory.sync_all() {
            tracing::warn!(%error, "panel password saved, but directory sync failed; check storage durability");
        }
        Ok(())
    })();
    drop(file);
    let _ = fs::remove_file(&temp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> std::path::PathBuf {
        let suffix = rand::random::<u128>();
        let path = std::env::temp_dir().join(format!("panel-password-{tag}-{suffix:032x}"));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn password_is_private_persistent_and_cannot_be_overwritten() {
        let root = root("save");
        let hash = crate::panel::auth::hash_password("a separate panel password").unwrap();
        assert!(load(&root).unwrap().is_none());
        save(&root, &hash).unwrap();
        assert_eq!(load(&root).unwrap().as_deref(), Some(hash.as_str()));
        assert!(save(&root, &hash).is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.join(PASSWORD_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_or_unknown_state_does_not_restore_legacy_login() {
        let root = root("invalid");
        for bytes in [
            "",
            "{}",
            "not json",
            r#"{"version":2,"password_hash":"bad"}"#,
        ] {
            fs::write(root.join(PASSWORD_FILE), bytes).unwrap();
            assert!(load(&root).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }
}

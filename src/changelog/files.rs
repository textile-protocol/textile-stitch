// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Where a config's change log lives, and opening those files safely.
//!
//! The log sits beside the config it records, not in a subdirectory:
//!
//! ```text
//! stitch.toml
//! change-log.jsonl      one ChangeRecord per line, append-only, never truncated
//! change-log.snapshot   the config as of the last record
//! change-log.shipped    how far into the journal Textile has acknowledged
//! change-log.refused    records Textile refused, one per line (rare)
//! change-log.lock       held while comparing, writing and recording
//! ```
//!
//! A flat-layout config (`stitch.<bot>.toml` beside others) prefixes them
//! with its own file name, so two bots never share a snapshot.
//!
//! In the panel's layout that directory belongs to the bot's uid while the
//! panel writes as root. The directory itself can't be swapped (its parent is
//! the panel's), but anything inside it can be. So every file here is opened
//! without following a link, must be a regular file with one name, and is
//! handed to the bot's uid through the open handle, never by path. A
//! subdirectory would need all of that for every component, which is why
//! there isn't one.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

/// One of a config's change-log files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFile {
    Journal,
    Snapshot,
    Shipped,
    Refused,
    Lock,
}

impl LogFile {
    fn suffix(self) -> &'static str {
        match self {
            LogFile::Journal => "jsonl",
            LogFile::Snapshot => "snapshot",
            LogFile::Shipped => "shipped",
            LogFile::Refused => "refused",
            LogFile::Lock => "lock",
        }
    }
}

/// The path of `file` for the config at `config_path`.
pub fn path(config_path: &Path, file: LogFile) -> PathBuf {
    let dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = match config_path.file_name().and_then(|n| n.to_str()) {
        Some("stitch.toml") | None => format!("change-log.{}", file.suffix()),
        Some(config) => format!("{config}.change-log.{}", file.suffix()),
    };
    dir.join(name)
}

/// Open for appending, creating it if needed, and give it to `owner`.
pub fn open_append(path: &Path, owner: Option<u32>) -> std::io::Result<File> {
    let file = open_regular(path, OpenOptions::new().create(true).append(true))?;
    hand_to(&file, owner)?;
    Ok(file)
}

/// Open the lock file, creating it if needed, and give it to `owner`.
pub fn open_lock(path: &Path, owner: Option<u32>) -> std::io::Result<File> {
    let file = open_regular(
        path,
        OpenOptions::new().create(true).truncate(false).write(true),
    )?;
    hand_to(&file, owner)?;
    Ok(file)
}

/// The whole file, or `None` when it doesn't exist.
pub fn read(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match open_regular(path, OpenOptions::new().read(true)) {
        Ok(mut file) => {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(Some(bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The whole file as text, or `None` when it doesn't exist.
pub fn read_text(path: &Path) -> std::io::Result<Option<String>> {
    read(path)?
        .map(|bytes| {
            String::from_utf8(bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })
        .transpose()
}

/// Open without following a link (`O_NOFOLLOW`) or blocking on a FIFO
/// (`O_NONBLOCK`), then refuse anything but a regular file with one name: a
/// hard link to someone else's file is as much a redirect as a symlink.
fn open_regular(path: &Path, opts: &mut OpenOptions) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = opts.open(path)?;
    let meta = file.metadata()?;
    let refuse = |why: &str| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} {why}, so the change log won't use it", path.display()),
        )
    };
    if !meta.file_type().is_file() {
        return Err(refuse("is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 {
            return Err(refuse("has more than one name"));
        }
    }
    Ok(file)
}

/// Give an open file to `owner` (the bot's uid) when it isn't theirs yet:
/// a root panel creates files the bot has to append to. Through the handle,
/// so whatever sits at the path by now doesn't matter.
#[cfg(unix)]
fn hand_to(file: &File, owner: Option<u32>) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    match owner {
        Some(uid) if file.metadata()?.uid() != uid => {
            std::os::unix::fs::fchown(file, Some(uid), None)
        }
        _ => Ok(()),
    }
}

#[cfg(not(unix))]
fn hand_to(_file: &File, _owner: Option<u32>) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stitch-changelog-files-{}-{tag}-{}",
            std::process::id(),
            crate::changelog::record::random_uuid()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_log_sits_beside_its_config() {
        let dir = Path::new("/data/bots/bot-a");
        assert_eq!(
            path(&dir.join("stitch.toml"), LogFile::Journal),
            dir.join("change-log.jsonl")
        );
        assert_eq!(
            path(&dir.join("stitch.toml"), LogFile::Snapshot),
            dir.join("change-log.snapshot")
        );
        let flat = Path::new("/data/bots");
        assert_eq!(
            path(&flat.join("stitch.bot-a.toml"), LogFile::Journal),
            flat.join("stitch.bot-a.toml.change-log.jsonl")
        );
        assert_ne!(
            path(&flat.join("stitch.bot-a.toml"), LogFile::Snapshot),
            path(&flat.join("stitch.bot-b.toml"), LogFile::Snapshot)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_is_never_followed() {
        let dir = temp_dir("symlink");
        let target = dir.join("someone-elses-file");
        std::fs::write(&target, "untouched").unwrap();
        let journal = dir.join("change-log.jsonl");
        std::os::unix::fs::symlink(&target, &journal).unwrap();

        assert!(open_append(&journal, None).is_err());
        assert!(open_lock(&journal, None).is_err());
        assert!(read(&journal).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_link_or_a_fifo_is_refused() {
        let dir = temp_dir("links");
        let target = dir.join("someone-elses-file");
        std::fs::write(&target, "untouched").unwrap();
        let journal = dir.join("change-log.jsonl");
        std::fs::hard_link(&target, &journal).unwrap();
        assert!(open_append(&journal, None).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");

        let fifo = dir.join("change-log.lock");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        // Would block forever on a plain open; refused straight away instead.
        assert!(open_lock(&fifo, None).is_err());
    }

    #[test]
    fn a_missing_file_reads_as_none() {
        let dir = temp_dir("missing");
        assert_eq!(read(&dir.join("change-log.jsonl")).unwrap(), None);
    }
}

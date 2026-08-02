//! One-shot import of a pre-panel `stitch-setup` config into the tray app's
//! `<data_root>/bots/<id>` tree so an upgrade doesn't open an empty fleet.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use stitch_bot::panel::naming::validate_bot_id;
use stitch_bot::setup::{
    default_dir, home_dir, is_configured, legacy_gui_dirs, remembered_config_dir,
};

use crate::paths::DesktopPaths;

const IMPORT_MARKER: &str = ".legacy-desktop-imported";

/// If the new bots root is empty of configured bots, copy the first legacy
/// desktop config we can find into `bots/<id>/`. Idempotent via a marker file.
pub fn import_legacy_desktop_config(paths: &DesktopPaths) -> Result<()> {
    import_with_candidates(paths, &legacy_candidates())
}

fn import_with_candidates(paths: &DesktopPaths, candidates: &[PathBuf]) -> Result<()> {
    let marker = paths.root.join(IMPORT_MARKER);
    if marker.is_file() {
        return Ok(());
    }
    if fleet_already_has_bot(&paths.bots_dir) {
        let _ = fs::write(&marker, "skipped-existing-fleet\n");
        return Ok(());
    }

    let Some(src) = candidates
        .iter()
        .find(|dir| is_configured(dir) && !dir.starts_with(&paths.bots_dir))
        .cloned()
    else {
        let _ = fs::write(&marker, "none-found\n");
        return Ok(());
    };

    let id = bot_id_for(&src);
    let dest = paths.bots_dir.join(&id);
    if dest.exists() {
        let _ = fs::write(&marker, format!("dest-exists:{}\n", dest.display()));
        return Ok(());
    }

    copy_dir_recursive(&src, &dest)
        .with_context(|| format!("importing legacy config from {}", src.display()))?;
    let _ = fs::write(
        &marker,
        format!("imported-from:{}\ninto:{}\n", src.display(), dest.display()),
    );
    eprintln!(
        "stitch-desktop: imported previous config from {} → {}",
        src.display(),
        dest.display()
    );
    Ok(())
}

fn fleet_already_has_bot(bots_dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(bots_dir) else {
        return false;
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                return false;
            }
            is_configured(e.path())
        })
}

fn legacy_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let push_unique = |out: &mut Vec<PathBuf>, p: PathBuf| {
        if !out.iter().any(|x| x == &p) {
            out.push(p);
        }
    };

    if let Some(remembered) = remembered_config_dir() {
        push_unique(&mut out, remembered);
    }
    // Linux: old pointer lived under ~/.config/stitch; new data root is
    // ~/.local/share/stitch — also read a pointer colocated with the data root.
    if let Some(home) = home_dir() {
        for dir in [
            home.join(".config/stitch"),
            home.join(".local/share/stitch"),
        ] {
            if let Ok(raw) = fs::read_to_string(dir.join("config-location")) {
                let trimmed = raw.trim();
                if !trimmed.is_empty() {
                    push_unique(&mut out, PathBuf::from(trimmed));
                }
            }
        }
        push_unique(&mut out, home.join("Stitch"));
    }
    push_unique(&mut out, default_dir());
    for legacy in legacy_gui_dirs() {
        push_unique(&mut out, legacy);
    }
    out
}

fn bot_id_for(src: &Path) -> String {
    let folder = src
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| "bot".into());
    let mut id: String = folder
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else if c == ' ' || c == '_' || c == '-' {
                '-'
            } else {
                '-'
            }
        })
        .collect();
    while id.contains("--") {
        id = id.replace("--", "-");
    }
    let id = id.trim_matches('-').to_string();
    if validate_bot_id(&id).is_ok() {
        id
    } else {
        "bot".into()
    }
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if ty.is_dir() {
            let name = entry.file_name();
            if name == ".git" || name == "target" || name == "node_modules" {
                continue;
            }
            copy_dir_recursive(&from, &to)?;
        } else if ty.is_file() {
            fs::copy(&from, &to)
                .with_context(|| format!("copying {} → {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stitch_bot::setup::config_paths;

    fn unique(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "stitch-desktop-migrate-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn desktop_paths(root: PathBuf) -> DesktopPaths {
        let bots_dir = root.join("bots");
        fs::create_dir_all(&bots_dir).unwrap();
        DesktopPaths {
            bots_dir,
            env_file: root.join("panel.env"),
            password_file: root.join("panel.password"),
            panel_log: root.join("panel.log"),
            root,
        }
    }

    #[test]
    fn imports_legacy_config_into_bots_root() {
        let root = unique("root");
        let paths = desktop_paths(root.clone());
        // Short folder name so bot_id_for keeps "my-stitch" (unique() tags are too long).
        let legacy_parent = unique("legacy-parent");
        let legacy = legacy_parent.join("My Stitch");
        fs::create_dir_all(&legacy).unwrap();
        let p = config_paths(&legacy);
        fs::write(&p.toml, "[bot]\nname = \"x\"\n").unwrap();
        fs::write(
            &p.key,
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80\n",
        )
        .unwrap();

        import_with_candidates(&paths, &[legacy.clone()]).unwrap();
        let dest = paths.bots_dir.join("my-stitch");
        assert!(
            is_configured(&dest),
            "expected import at {}",
            dest.display()
        );
        assert!(root.join(IMPORT_MARKER).is_file());

        // Idempotent.
        import_with_candidates(&paths, &[legacy.clone()]).unwrap();

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&legacy_parent);
    }

    #[test]
    fn skips_when_fleet_already_populated() {
        let root = unique("populated");
        let paths = desktop_paths(root.clone());
        let existing = paths.bots_dir.join("alpha");
        fs::create_dir_all(&existing).unwrap();
        let p = config_paths(&existing);
        fs::write(&p.toml, "[bot]\n").unwrap();
        fs::write(&p.key, "x").unwrap();

        let legacy = unique("legacy");
        let lp = config_paths(&legacy);
        fs::write(&lp.toml, "[bot]\n").unwrap();
        fs::write(&lp.key, "y").unwrap();

        import_with_candidates(&paths, &[legacy.clone()]).unwrap();
        assert!(!paths.bots_dir.join("legacy").exists());
        assert_eq!(
            fs::read_to_string(root.join(IMPORT_MARKER)).unwrap().trim(),
            "skipped-existing-fleet"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&legacy);
    }

    #[test]
    fn bot_id_sanitizes_folder_names() {
        assert_eq!(bot_id_for(Path::new("/tmp/My Stitch")), "my-stitch");
        assert_eq!(bot_id_for(Path::new("/tmp/!!!")), "bot");
    }
}

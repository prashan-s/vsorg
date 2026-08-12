//! Snapshots the VS Code user directory before any mutation.
//!
//! Profile deletion is not undoable and takes settings, keybindings, snippets, tasks and UI state
//! with it, so `apply` always writes one of these first.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;

/// Directories excluded from the archive: bulky, regenerable, and irrelevant to profile identity.
/// On a typical install these dwarf everything else (thousands of entries each).
const EXCLUDE: [&str; 3] = ["History", "workspaceStorage", "logs"];

/// Archive `user_dir` into `dest_dir`, returning the archive path.
pub fn snapshot(user_dir: &Path, dest_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating backup directory {}", dest_dir.display()))?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dest_dir.join(format!("vscode-user-{stamp}.tar.gz"));

    let file = File::create(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    let enc = GzEncoder::new(BufWriter::new(file), Compression::default());
    let mut tar = tar::Builder::new(enc);

    for entry in fs::read_dir(user_dir)
        .with_context(|| format!("reading {}", user_dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        if EXCLUDE.contains(&name_str.as_str()) {
            continue;
        }
        let path_in = entry.path();
        let rel = Path::new("User").join(&name);
        if path_in.is_dir() {
            tar.append_dir_all(&rel, &path_in)
                .with_context(|| format!("archiving {}", path_in.display()))?;
        } else {
            tar.append_path_with_name(&path_in, &rel)
                .with_context(|| format!("archiving {}", path_in.display()))?;
        }
    }

    tar.into_inner()
        .context("finalising tar")?
        .finish()
        .context("finalising gzip")?;

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_the_bulky_regenerable_directories() {
        // Guards against someone adding History back and producing multi-GB backups.
        assert!(EXCLUDE.contains(&"History"));
        assert!(EXCLUDE.contains(&"workspaceStorage"));
    }

    #[test]
    fn snapshot_skips_excluded_dirs_and_keeps_the_rest() {
        let root = std::env::temp_dir().join(format!("vsorg-backup-test-{}", std::process::id()));
        let user = root.join("User");
        fs::create_dir_all(user.join("History")).unwrap();
        fs::create_dir_all(user.join("snippets")).unwrap();
        fs::write(user.join("settings.json"), "{}").unwrap();
        fs::write(user.join("History").join("big.bin"), "x").unwrap();
        fs::write(user.join("snippets").join("go.json"), "{}").unwrap();

        let archive = snapshot(&user, &root.join("out")).unwrap();
        let decoder = flate2::read::GzDecoder::new(File::open(&archive).unwrap());
        let names: Vec<String> = tar::Archive::new(decoder)
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().display().to_string())
            .collect();

        assert!(names.iter().any(|n| n.ends_with("settings.json")));
        assert!(names.iter().any(|n| n.ends_with("snippets/go.json")));
        assert!(!names.iter().any(|n| n.contains("History")));

        fs::remove_dir_all(&root).ok();
    }
}

//! Restores a snapshot written by [`crate::backup`].
//!
//! The archive holds `User/` — `storage.json`, per-profile directories, settings, keybindings,
//! snippets, tasks. It does **not** hold extension binaries: those live in `~/.vscode/extensions`,
//! are shared across profiles, and are refetched by VS Code or by `vsorg apply`.
//!
//! Restoring is itself destructive, so it snapshots the current state first, refuses to run while
//! VS Code is open (the editor rewrites `storage.json` on exit and would undo the restore), and
//! treats archive paths as hostile.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;

/// One file in the archive, with its path already resolved against the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the user directory, e.g. `globalStorage/storage.json`.
    pub relative: PathBuf,
    pub size: u64,
}

/// Read the archive without writing anything.
pub fn inspect(archive: &Path) -> Result<Vec<Entry>> {
    let mut out = Vec::new();

    for entry in open(archive)?.entries().context("reading archive entries")? {
        let entry = entry.context("reading archive entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().context("archive entry path")?.into_owned();
        let Some(relative) = strip_user_prefix(&path)? else { continue };
        out.push(Entry { relative, size: entry.header().size().unwrap_or(0) });
    }

    if out.is_empty() {
        bail!(
            "{} contains no files under `User/` — is it a vsorg backup?",
            archive.display()
        );
    }

    out.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(out)
}

/// Extract the archive over `user_dir`.
///
/// Files present in the archive are overwritten; files created since the backup are **left in
/// place**. That asymmetry matters: a profile created after the snapshot keeps its directory but
/// vanishes from the restored `storage.json`, so VS Code stops listing it. Nothing is deleted, so
/// it can be recovered by restoring the newer snapshot instead.
pub fn extract(archive: &Path, user_dir: &Path) -> Result<Vec<Entry>> {
    let planned = inspect(archive)?;

    for entry in open(archive)?.entries().context("reading archive entries")? {
        let mut entry = entry.context("reading archive entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().context("archive entry path")?.into_owned();
        let Some(relative) = strip_user_prefix(&path)? else { continue };

        let dest = user_dir.join(&relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        entry
            .unpack(&dest)
            .with_context(|| format!("extracting {}", dest.display()))?;
    }

    Ok(planned)
}

/// Which profile directories the archive would bring back that are not currently registered.
///
/// The interesting case for a restore: `storage.json` lost its `userDataProfiles` entries while
/// the directories survived on disk. Naming them tells the user what they are about to get back.
pub fn restored_profile_dirs(entries: &[Entry]) -> BTreeSet<String> {
    entries
        .iter()
        .filter_map(|e| {
            let mut parts = e.relative.components();
            if parts.next()?.as_os_str() != "profiles" {
                return None;
            }
            Some(parts.next()?.as_os_str().to_string_lossy().to_string())
        })
        .collect()
}

/// Where snapshots are looked for when no archive is named, relative to the working directory.
///
/// `vsorg-backups` is what `apply` and `backup` write to by default; `.` covers running the
/// command from inside that directory, or having dropped an archive beside you.
pub const SEARCH_DIRS: [&str; 2] = ["vsorg-backups", "."];

/// Find a snapshot to restore.
///
/// With an explicit directory, that directory is authoritative and its absence is an error. With
/// none, [`SEARCH_DIRS`] are tried in order. Returns the archive and the directory it came from,
/// so the caller can say where it looked rather than restoring from a surprising place silently.
pub fn discover(explicit_dir: Option<&Path>) -> Result<(PathBuf, PathBuf)> {
    if let Some(dir) = explicit_dir {
        if !dir.is_dir() {
            bail!("{} is not a directory", dir.display());
        }
        return Ok((latest_in(dir)?, dir.to_path_buf()));
    }

    for name in SEARCH_DIRS {
        let dir = PathBuf::from(name);
        if !dir.is_dir() {
            continue;
        }
        if let Ok(archive) = latest_in(&dir) {
            return Ok((archive, dir));
        }
    }

    Err(anyhow!(
        "no snapshot found. Looked for vscode-user-*.tar.gz in: {}\n\
         Pass an archive path, or --backup-dir <DIR>.",
        SEARCH_DIRS
            .iter()
            .map(|d| if *d == "." { "./".to_string() } else { format!("./{d}") })
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// The newest `vscode-user-*.tar.gz` in a directory.
pub fn latest_in(dir: &Path) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("vscode-user-") && n.ends_with(".tar.gz"))
        })
        .collect();

    // Names carry a unix timestamp, so lexicographic order is chronological — and unlike mtime it
    // survives a copy.
    candidates.sort();

    candidates
        .pop()
        .ok_or_else(|| anyhow!("no vscode-user-*.tar.gz backups found in {}", dir.display()))
}

fn open(archive: &Path) -> Result<tar::Archive<GzDecoder<BufReader<File>>>> {
    let file = File::open(archive)
        .with_context(|| format!("opening {}", archive.display()))?;
    Ok(tar::Archive::new(GzDecoder::new(BufReader::new(file))))
}

/// Strip the archive's leading `User/` and reject anything that would escape the destination.
///
/// Returns `None` for entries outside `User/`, which are simply skipped. Errors on traversal:
/// a crafted archive must never be able to write over `~/.ssh/authorized_keys` because someone
/// pointed `vsorg restore` at a file they downloaded.
fn strip_user_prefix(path: &Path) -> Result<Option<PathBuf>> {
    let mut parts = path.components();

    match parts.next() {
        Some(Component::Normal(first)) if first == "User" => {}
        _ => return Ok(None),
    }

    let mut relative = PathBuf::new();
    for part in parts {
        match part {
            Component::Normal(p) => relative.push(p),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "archive entry `{}` escapes the destination directory; refusing to extract it",
                    path.display()
                )
            }
        }
    }

    if relative.as_os_str().is_empty() {
        return Ok(None);
    }
    Ok(Some(relative))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir()
            .join(format!("vsorg-restore-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Build a `.tar.gz` with the given `path -> contents` entries.
    ///
    /// Names are written straight into the raw header rather than through `set_path`, because the
    /// `tar` crate refuses to *construct* an entry containing `..` — which would make it
    /// impossible to test that we reject one on the way *out*. A real attacker has no such
    /// scruples, so neither does this helper.
    fn archive_with(dir: &Path, entries: &[(&str, &str)]) -> PathBuf {
        let path = dir.join("vscode-user-1700000000.tar.gz");
        let enc = flate2::write::GzEncoder::new(
            File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(enc);

        for (name, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            let raw = name.as_bytes();
            header.as_old_mut().name[..raw.len()].copy_from_slice(raw);
            header.set_cksum();
            tar.append(&header, body.as_bytes()).unwrap();
        }

        tar.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn round_trips_a_snapshot_written_by_the_backup_module() {
        let root = temp("roundtrip");
        let user = root.join("User");
        fs::create_dir_all(user.join("profiles/-abc")).unwrap();
        fs::create_dir_all(user.join("globalStorage")).unwrap();
        fs::write(user.join("settings.json"), r#"{"a":1}"#).unwrap();
        fs::write(user.join("globalStorage/storage.json"), r#"{"userDataProfiles":[]}"#).unwrap();
        fs::write(user.join("profiles/-abc/extensions.json"), "[]").unwrap();

        let archive = crate::backup::snapshot(&user, &root.join("out")).unwrap();

        // Simulate the loss this command exists for: storage.json overwritten.
        fs::write(user.join("globalStorage/storage.json"), "{}").unwrap();
        fs::remove_file(user.join("settings.json")).unwrap();

        let restored = extract(&archive, &user).unwrap();

        assert_eq!(
            fs::read_to_string(user.join("globalStorage/storage.json")).unwrap(),
            r#"{"userDataProfiles":[]}"#
        );
        assert_eq!(fs::read_to_string(user.join("settings.json")).unwrap(), r#"{"a":1}"#);
        assert!(restored.iter().any(|e| e.relative == Path::new("settings.json")));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn names_the_profile_directories_a_restore_brings_back() {
        let entries = vec![
            Entry { relative: PathBuf::from("settings.json"), size: 2 },
            Entry { relative: PathBuf::from("profiles/-29f7c7e6/extensions.json"), size: 2 },
            Entry { relative: PathBuf::from("profiles/-29f7c7e6/settings.json"), size: 2 },
            Entry { relative: PathBuf::from("profiles/-529b84bd/extensions.json"), size: 2 },
        ];
        let dirs = restored_profile_dirs(&entries);
        assert_eq!(dirs.len(), 2);
        assert!(dirs.contains("-29f7c7e6"));
        assert!(dirs.contains("-529b84bd"));
    }

    #[test]
    fn refuses_archive_entries_that_escape_the_destination() {
        // The zip-slip case: `vsorg restore` takes a path, so an untrusted archive must not be
        // able to write outside User/.
        let err = strip_user_prefix(Path::new("User/../../.ssh/authorized_keys")).unwrap_err();
        assert!(err.to_string().contains("escapes the destination"), "{err}");
        assert!(strip_user_prefix(Path::new("User/../evil")).is_err());
        assert!(strip_user_prefix(Path::new("User/a/../../evil")).is_err());
    }

    #[test]
    fn ignores_entries_outside_the_user_directory() {
        assert_eq!(strip_user_prefix(Path::new("other/thing.json")).unwrap(), None);
        assert_eq!(strip_user_prefix(Path::new("User")).unwrap(), None);
        assert_eq!(
            strip_user_prefix(Path::new("User/./globalStorage/storage.json")).unwrap(),
            Some(PathBuf::from("globalStorage/storage.json"))
        );
    }

    #[test]
    fn an_archive_with_nothing_restorable_is_an_error_not_a_silent_no_op() {
        let root = temp("empty");
        let archive = archive_with(&root, &[("some/other/file.txt", "hi")]);
        let err = inspect(&archive).unwrap_err();
        assert!(err.to_string().contains("no files under `User/`"), "{err}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_traversal_entry_is_caught_before_anything_is_written() {
        let root = temp("traversal");
        let archive = archive_with(&root, &[("User/../escaped.txt", "pwned")]);
        assert!(inspect(&archive).is_err());

        // extract() inspects first, so the write never begins.
        let user = root.join("User");
        fs::create_dir_all(&user).unwrap();
        assert!(extract(&archive, &user).is_err());
        assert!(!root.join("escaped.txt").exists());

        fs::remove_dir_all(&root).ok();
    }

    /// `discover` searches relative to the process working directory, which is per-process, not
    /// per-thread — so these must not run concurrently with each other.
    fn with_cwd<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        let out = f();
        std::env::set_current_dir(original).unwrap();
        out
    }

    #[test]
    fn discovers_a_backup_dir_beside_the_working_directory() {
        let root = temp("discover-subdir");
        let backups = root.join("vsorg-backups");
        fs::create_dir_all(&backups).unwrap();
        archive_with(&backups, &[("User/settings.json", "{}")]);

        let (archive, dir) = with_cwd(&root, || discover(None)).unwrap();
        assert_eq!(dir, Path::new("vsorg-backups"));
        assert!(archive.to_string_lossy().ends_with("vscode-user-1700000000.tar.gz"));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn falls_back_to_archives_sitting_in_the_working_directory() {
        // Covers running the command from inside the backup directory itself.
        let root = temp("discover-cwd");
        archive_with(&root, &[("User/settings.json", "{}")]);

        let (_, dir) = with_cwd(&root, || discover(None)).unwrap();
        assert_eq!(dir, Path::new("."));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discovery_failure_names_everywhere_it_looked() {
        let root = temp("discover-none");
        let err = with_cwd(&root, || discover(None)).unwrap_err().to_string();
        assert!(err.contains("vsorg-backups"), "{err}");
        assert!(err.contains("--backup-dir"), "{err}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_explicit_backup_dir_is_authoritative() {
        // Must not silently fall through to ./ when the named directory is wrong.
        let root = temp("discover-explicit");
        archive_with(&root, &[("User/settings.json", "{}")]);
        let missing = root.join("nope");

        let err = with_cwd(&root, || discover(Some(&missing))).unwrap_err().to_string();
        assert!(err.contains("not a directory"), "{err}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn latest_in_picks_the_newest_by_timestamp_name() {
        let root = temp("latest");
        for stamp in ["1700000000", "1800000000", "1750000000"] {
            let mut f = File::create(root.join(format!("vscode-user-{stamp}.tar.gz"))).unwrap();
            f.write_all(b"x").unwrap();
        }
        // Unrelated files must not be picked up.
        File::create(root.join("notes.txt")).unwrap();

        let latest = latest_in(&root).unwrap();
        assert!(latest.to_string_lossy().ends_with("vscode-user-1800000000.tar.gz"), "{latest:?}");

        fs::remove_dir_all(&root).ok();
        let empty = temp("latest-empty");
        assert!(latest_in(&empty).is_err());
        fs::remove_dir_all(&empty).ok();
    }
}

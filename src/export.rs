//! Inventory capture and manifest reverse-engineering.
//!
//! [`export`] is the pre-migration safety net: profile deletion is irreversible and the CLI has no
//! import path for settings, keybindings, snippets or tasks. [`derive_manifest`] turns the live
//! install into a manifest so `plan` against it is a no-op — the starting point for editing.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::manifest::{Manifest, Meta, ProfileSpec};
use crate::paths::Layout;
use crate::state::{State, DEFAULT_PROFILE_NAME};
use crate::store::Inventory;

/// Per-profile files worth preserving. Directories are copied recursively.
const PROFILE_FILES: [&str; 4] =
    ["settings.json", "keybindings.json", "tasks.json", "chatLanguageModels.json"];
const PROFILE_DIRS: [&str; 2] = ["snippets", "prompts"];

/// Write one directory per profile containing its extension list and content files.
/// Returns the per-profile extension counts, in the order written.
pub fn export(layout: &Layout, state: &State, inv: &Inventory, dest: &Path) -> Result<Vec<(String, usize)>> {
    fs::create_dir_all(dest)
        .with_context(|| format!("creating {}", dest.display()))?;

    let mut summary = Vec::new();

    for pe in &inv.profiles {
        let dir = dest.join(sanitize(&pe.profile));
        fs::create_dir_all(&dir)?;

        // One ID per line: directly consumable by `xargs -n1 code --install-extension`.
        let list: String = pe.extensions.iter().map(|e| format!("{}\n", e.id)).collect();
        fs::write(dir.join("extensions.txt"), &list)?;

        // Versions matter when rolling back to a known-good state.
        let pinned: String = pe
            .extensions
            .iter()
            .map(|e| format!("{}@{}\n", e.id, e.version.as_deref().unwrap_or("unknown")))
            .collect();
        fs::write(dir.join("extensions-pinned.txt"), &pinned)?;

        // Default's content files live directly in User/; named profiles have their own dir.
        let src = match &pe.location {
            Some(loc) => layout.profile_dir(loc),
            None => layout.user_dir.clone(),
        };
        copy_content(&src, &dir)?;

        summary.push((pe.profile.clone(), pe.extensions.len()));
    }

    // The bindings are not recoverable from the profiles themselves.
    let mut bindings = String::from("# workspace\tprofile\n");
    let by_location: BTreeMap<&str, &str> = state
        .profiles
        .iter()
        .map(|p| (p.location.as_str(), p.name.as_str()))
        .collect();
    for (ws, loc) in &state.workspaces {
        let name = by_location.get(loc.as_str()).copied().unwrap_or(DEFAULT_PROFILE_NAME);
        bindings.push_str(&format!("{ws}\t{name}\n"));
    }
    fs::write(dest.join("workspace-bindings.tsv"), bindings)?;

    Ok(summary)
}

fn copy_content(src: &Path, dest: &Path) -> Result<()> {
    for f in PROFILE_FILES {
        let from = src.join(f);
        if from.is_file() {
            fs::copy(&from, dest.join(f))
                .with_context(|| format!("copying {}", from.display()))?;
        }
    }
    for d in PROFILE_DIRS {
        let from = src.join(d);
        if from.is_dir() {
            copy_dir(&from, &dest.join(d))?;
        }
    }
    Ok(())
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || "-_.".contains(c) { c } else { '_' })
        .collect()
}

/// Build a manifest describing the current install exactly.
///
/// `base` is the intersection across every profile that has at least one extension. Profiles with
/// none are skipped when intersecting — an empty profile would otherwise collapse the base to
/// nothing. With fewer than two non-empty profiles there is nothing to intersect, so `base` is
/// left empty rather than guessed.
pub fn derive_manifest(state: &State, inv: &Inventory) -> Manifest {
    let sets: Vec<(String, BTreeSet<String>)> = inv
        .profiles
        .iter()
        .filter(|p| {
            // Built-in profiles are not ours to manage.
            p.location.as_deref().map(|l| !l.starts_with("builtin/")).unwrap_or(true)
        })
        .map(|p| (p.profile.clone(), p.ids()))
        .collect();

    let non_empty: Vec<&BTreeSet<String>> =
        sets.iter().map(|(_, s)| s).filter(|s| !s.is_empty()).collect();

    let base: BTreeSet<String> = if non_empty.len() < 2 {
        BTreeSet::new()
    } else {
        non_empty
            .iter()
            .skip(1)
            .fold(non_empty[0].clone(), |acc, s| acc.intersection(s).cloned().collect())
    };

    let mut profiles = BTreeMap::new();
    for (name, set) in &sets {
        let own: Vec<String> = set.difference(&base).cloned().collect();
        let shared = state
            .find(name)
            .map(|e| e.flags().enabled().iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        profiles.insert(
            name.to_ascii_lowercase(),
            ProfileSpec {
                description: None,
                extensions: own,
                // Derived manifests describe reality, so pruning is a no-op — but enabling it
                // makes the file authoritative from here on.
                prune: true,
                shared,
                no_base: false,
            },
        );
    }

    Manifest {
        meta: Meta { version: 1 },
        base: base.into_iter().collect(),
        ignore: Vec::new(),
        profiles,
    }
}

/// Suggested output path when the user gives none.
pub fn default_manifest_path() -> PathBuf {
    PathBuf::from("vscode-organizer.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{InstalledExtension, ProfileExtensions};

    fn inv(profiles: &[(&str, &[&str])]) -> Inventory {
        Inventory {
            profiles: profiles
                .iter()
                .map(|(name, ids)| ProfileExtensions {
                    profile: name.to_string(),
                    location: if *name == "Default" { None } else { Some(format!("-{name}")) },
                    extensions: ids
                        .iter()
                        .map(|id| InstalledExtension {
                            id: id.to_string(),
                            version: None,
                            relative_location: None,
                        })
                        .collect(),
                })
                .collect(),
            on_disk: BTreeSet::new(),
            packs: BTreeMap::new(),
            facts: BTreeMap::new(),
        }
    }

    fn empty_state() -> State {
        State::parse("{}").unwrap()
    }

    #[test]
    fn base_is_the_intersection_across_non_empty_profiles() {
        let i = inv(&[
            ("Default", &["shared.a", "shared.b", "only.default"]),
            ("Go", &["shared.a", "shared.b", "golang.go"]),
        ]);
        let m = derive_manifest(&empty_state(), &i);
        assert_eq!(m.base, vec!["shared.a", "shared.b"]);
        assert_eq!(m.profiles["go"].extensions, vec!["golang.go"]);
        assert_eq!(m.profiles["default"].extensions, vec!["only.default"]);
    }

    #[test]
    fn an_empty_profile_does_not_collapse_the_base() {
        let i = inv(&[
            ("Default", &["shared.a"]),
            ("Go", &["shared.a", "golang.go"]),
            ("Empty", &[]),
        ]);
        let m = derive_manifest(&empty_state(), &i);
        assert_eq!(m.base, vec!["shared.a"]);
        assert!(m.profiles["empty"].extensions.is_empty());
    }

    #[test]
    fn a_single_profile_gets_no_inferred_base() {
        // Nothing to intersect against; inventing a base from one profile would be a guess.
        let m = derive_manifest(&empty_state(), &inv(&[("Default", &["a.a", "b.b"])]));
        assert!(m.base.is_empty());
        assert_eq!(m.profiles["default"].extensions, vec!["a.a", "b.b"]);
    }

    #[test]
    fn derived_manifest_reproduces_each_profiles_exact_set() {
        let i = inv(&[
            ("Default", &["shared.a", "x.x"]),
            ("Go", &["shared.a", "golang.go"]),
        ]);
        let m = derive_manifest(&empty_state(), &i);
        for pe in &i.profiles {
            assert_eq!(
                m.desired(&pe.profile.to_ascii_lowercase()).unwrap(),
                pe.ids(),
                "round-trip must be exact for {}",
                pe.profile
            );
        }
    }

    #[test]
    fn shared_flags_are_carried_over_from_live_state() {
        let s = State::parse(
            r#"{"userDataProfiles":[{"location":"-Go","name":"Go",
                "useDefaultFlags":{"keybindings":true,"snippets":true}}]}"#,
        )
        .unwrap();
        let m = derive_manifest(&s, &inv(&[("Go", &["golang.go"])]));
        assert_eq!(m.profiles["go"].shared, vec!["keybindings", "snippets"]);
    }

    #[test]
    fn builtin_profiles_are_omitted() {
        let mut i = inv(&[("Default", &["a.a"])]);
        i.profiles.push(ProfileExtensions {
            profile: "Agents".into(),
            location: Some("builtin/agents".into()),
            extensions: Vec::new(),
        });
        let m = derive_manifest(&empty_state(), &i);
        assert!(!m.profiles.contains_key("agents"));
    }
}

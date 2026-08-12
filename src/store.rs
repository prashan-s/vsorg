//! Reads extension manifests and the shared on-disk extension store.
//!
//! Three distinct sets, easily conflated:
//!
//! 1. **On disk** — the `<pub>.<name>-<ver>/` folders under `~/.vscode/extensions`. Shared by
//!    every profile; deleting a profile frees metadata, not disk.
//! 2. **Default's manifest** — `~/.vscode/extensions/extensions.json`.
//! 3. **A named profile's manifest** — `User/profiles/<loc>/extensions.json`.
//!
//! (2) is *not* a superset of (3): an extension used only by a named profile is on disk and in
//! that profile's manifest while being absent from Default's. Anything on disk that appears in no
//! manifest is an orphan.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::paths::Layout;
use crate::state::{ProfileEntry, State, DEFAULT_PROFILE_NAME};

#[derive(Debug, Clone, Deserialize)]
struct Identifier {
    id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ManifestEntry {
    identifier: Identifier,
    #[serde(default)]
    version: Option<String>,
    #[serde(rename = "relativeLocation", default)]
    relative_location: Option<String>,
}

/// One extension as recorded in a profile manifest. IDs are normalised to lowercase because VS
/// Code's marketplace treats them case-insensitively while the files preserve author casing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledExtension {
    pub id: String,
    pub version: Option<String>,
    pub relative_location: Option<String>,
}

/// The resolved extension set of a single profile.
#[derive(Debug, Clone)]
pub struct ProfileExtensions {
    pub profile: String,
    /// `None` for Default, which has no `storage.json` entry.
    pub location: Option<String>,
    pub extensions: Vec<InstalledExtension>,
}

impl ProfileExtensions {
    pub fn ids(&self) -> BTreeSet<String> {
        self.extensions.iter().map(|e| e.id.clone()).collect()
    }
}

/// Parse a manifest file. A missing file is an empty profile, not an error: VS Code creates
/// `extensions.json` lazily on first install.
pub fn read_manifest(path: &Path) -> Result<Vec<InstalledExtension>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_manifest(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn parse_manifest(text: &str) -> Result<Vec<InstalledExtension>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let raw: Vec<ManifestEntry> = serde_json::from_str(text)?;
    let mut out: Vec<InstalledExtension> = raw
        .into_iter()
        .map(|e| InstalledExtension {
            id: e.identifier.id.to_ascii_lowercase(),
            version: e.version,
            relative_location: e.relative_location,
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    Ok(out)
}

/// What an extension's own `package.json` says about itself.
///
/// The classifier ([`crate::classify`]) runs on this rather than on IDs: `sweetpad.sweetpad`
/// tells you nothing, but "SweetPad (iOS/Swift development)" plus its categories does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionFacts {
    pub id: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    /// Marketplace categories, e.g. `Programming Languages`, `Linters`, `Extension Packs`.
    pub categories: Vec<String>,
    pub keywords: Vec<String>,
    /// `contributes.languages[].id` — the strongest toolchain signal available.
    pub languages: Vec<String>,
    /// `contributes.debuggers[].type`.
    pub debuggers: Vec<String>,
    pub pack: Vec<String>,
    /// `extensionDependencies` — these must land in the same profile or activation fails.
    pub depends: Vec<String>,
}

/// Everything read off disk in one pass, for every profile including Default.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub profiles: Vec<ProfileExtensions>,
    /// Extension IDs with a folder under `~/.vscode/extensions`, regardless of manifest presence.
    pub on_disk: BTreeSet<String>,
    /// Pack ID → member IDs, harvested from each on-disk `package.json`.
    pub packs: BTreeMap<String, Vec<String>>,
    /// Per-extension metadata, keyed by ID. Missing for folders with an unreadable `package.json`.
    pub facts: BTreeMap<String, ExtensionFacts>,
}

impl Inventory {
    pub fn load(layout: &Layout, state: &State) -> Result<Inventory> {
        let mut profiles = vec![ProfileExtensions {
            profile: DEFAULT_PROFILE_NAME.to_string(),
            location: None,
            extensions: read_manifest(&layout.default_profile_extensions_json())?,
        }];

        for p in state.profiles.iter() {
            profiles.push(read_profile(layout, p)?);
        }

        let facts = scan_extensions_dir(&layout.extensions_dir)?;
        let on_disk = facts.keys().cloned().collect();
        let packs = facts
            .iter()
            .filter(|(_, f)| !f.pack.is_empty())
            .map(|(id, f)| (id.clone(), f.pack.clone()))
            .collect();

        Ok(Inventory { profiles, on_disk, packs, facts })
    }

    pub fn get(&self, profile: &str) -> Option<&ProfileExtensions> {
        self.profiles.iter().find(|p| p.profile.eq_ignore_ascii_case(profile))
    }

    /// Union of every profile manifest. On a healthy install this equals [`Self::on_disk`].
    pub fn referenced(&self) -> BTreeSet<String> {
        self.profiles.iter().flat_map(|p| p.ids()).collect()
    }

    /// Present on disk but referenced by no profile — dead weight left by profile deletion or a
    /// failed uninstall.
    pub fn orphans(&self) -> BTreeSet<String> {
        let referenced = self.referenced();
        self.on_disk.difference(&referenced).cloned().collect()
    }

    /// For each declared pack, the members that were *not* also declared.
    ///
    /// Packs expand at install time, so an undeclared member appears in the profile anyway. Under
    /// `prune` that means an endless uninstall/reinstall cycle; without it, silent drift. Note
    /// this is not "replace the pack with its members": `ms-python.python` is a genuine extension
    /// that also declares a pack, so dropping it would remove the Python support itself. Declare
    /// both.
    pub fn unpinned_pack_members(
        &self,
        declared: &BTreeSet<String>,
    ) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::new();
        for (pack, members) in &self.packs {
            if !declared.contains(pack) {
                continue;
            }
            let missing: Vec<String> = members
                .iter()
                .filter(|m| !declared.contains(*m))
                .cloned()
                .collect();
            if !missing.is_empty() {
                out.insert(pack.clone(), missing);
            }
        }
        out
    }

    /// Referenced by a profile but absent from disk — a broken manifest; VS Code will fail to
    /// activate these.
    pub fn dangling(&self) -> BTreeSet<String> {
        self.referenced().difference(&self.on_disk).cloned().collect()
    }
}

fn read_profile(layout: &Layout, p: &ProfileEntry) -> Result<ProfileExtensions> {
    // A profile with `useDefaultFlags.extensions` inherits Default's set live, so its own
    // manifest is meaningless — read Default's instead.
    let path = if p.flags().extensions {
        layout.default_profile_extensions_json()
    } else {
        layout.profile_extensions_json(&p.location)
    };
    Ok(ProfileExtensions {
        profile: p.name.clone(),
        location: Some(p.location.clone()),
        extensions: read_manifest(&path)?,
    })
}

#[derive(Debug, Default, Deserialize)]
struct Contributes {
    #[serde(default)]
    languages: Vec<LanguageContribution>,
    #[serde(default)]
    debuggers: Vec<DebuggerContribution>,
}

#[derive(Debug, Deserialize)]
struct LanguageContribution {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DebuggerContribution {
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PackageJson {
    #[serde(default)]
    publisher: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    // Some manifests set these to `null` rather than omitting them, which a bare `Vec<String>`
    // would reject and take the whole extension's metadata down with it.
    #[serde(default)]
    categories: Option<Vec<String>>,
    #[serde(default)]
    keywords: Option<Vec<String>>,
    #[serde(rename = "extensionPack", default)]
    extension_pack: Option<Vec<String>>,
    #[serde(rename = "extensionDependencies", default)]
    extension_dependencies: Option<Vec<String>>,
    #[serde(default)]
    contributes: Option<Contributes>,
}

/// Walk `~/.vscode/extensions`, deriving each extension's ID from its `package.json` (falling back
/// to the folder name) and harvesting the metadata the classifier reasons over.
fn scan_extensions_dir(dir: &Path) -> Result<BTreeMap<String, ExtensionFacts>> {
    let mut out: BTreeMap<String, ExtensionFacts> = BTreeMap::new();

    if !dir.is_dir() {
        return Ok(out);
    }

    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let folder = entry.file_name().to_string_lossy().to_string();
        if folder.starts_with('.') {
            continue;
        }

        let pkg_path = entry.path().join("package.json");
        let parsed: Option<PackageJson> = fs::read_to_string(&pkg_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok());

        let id = match &parsed {
            Some(PackageJson { publisher: Some(pubr), name: Some(name), .. }) => {
                format!("{pubr}.{name}").to_ascii_lowercase()
            }
            // Folders are `<pub>.<name>-<semver>`; strip the trailing version.
            _ => strip_version_suffix(&folder).to_ascii_lowercase(),
        };

        let pkg = parsed.unwrap_or_default();
        let contributes = pkg.contributes.unwrap_or_default();

        let facts = ExtensionFacts {
            id: id.clone(),
            display_name: pkg.display_name,
            description: pkg.description,
            categories: pkg.categories.unwrap_or_default(),
            keywords: pkg.keywords.unwrap_or_default(),
            languages: contributes.languages.into_iter().filter_map(|l| l.id).collect(),
            debuggers: contributes.debuggers.into_iter().filter_map(|d| d.kind).collect(),
            pack: lower(pkg.extension_pack),
            depends: lower(pkg.extension_dependencies),
        };

        // Several versions of the same extension can coexist on disk mid-upgrade; the richer
        // manifest wins so a stale stub cannot blank out the metadata.
        match out.get(&id) {
            Some(existing) if score(existing) >= score(&facts) => {}
            _ => {
                out.insert(id, facts);
            }
        }
    }

    Ok(out)
}

fn lower(v: Option<Vec<String>>) -> Vec<String> {
    v.unwrap_or_default().iter().map(|s| s.to_ascii_lowercase()).collect()
}

/// How much the classifier can learn from this record.
fn score(f: &ExtensionFacts) -> usize {
    usize::from(f.display_name.is_some())
        + usize::from(f.description.is_some())
        + f.categories.len()
        + f.languages.len()
        + f.pack.len()
}

/// `ms-python.python-2025.4.0` → `ms-python.python`. Only strips a trailing `-<digit>…` segment so
/// hyphenated extension names survive.
fn strip_version_suffix(folder: &str) -> &str {
    match folder.rfind('-') {
        Some(idx) if folder[idx + 1..].starts_with(|c: char| c.is_ascii_digit()) => &folder[..idx],
        _ => folder,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Entry shape captured verbatim from a real install.
    const MANIFEST: &str = r#"[
      { "identifier": { "id": "nhoizey.gremlins", "uuid": "0fce" },
        "version": "0.26.0",
        "location": { "$mid": 1, "path": "/x/nhoizey.gremlins-0.26.0", "scheme": "file" },
        "relativeLocation": "nhoizey.gremlins-0.26.0",
        "metadata": { "installedTimestamp": 1748940549947, "pinned": false } },
      { "identifier": { "id": "EditorConfig.EditorConfig" },
        "version": "0.17.4",
        "relativeLocation": "editorconfig.editorconfig-0.17.4" }
    ]"#;

    #[test]
    fn parses_real_manifest_shape_and_lowercases_ids() {
        let v = parse_manifest(MANIFEST).unwrap();
        assert_eq!(v.len(), 2);
        // Sorted, and the mixed-case marketplace ID is normalised.
        assert_eq!(v[0].id, "editorconfig.editorconfig");
        assert_eq!(v[1].id, "nhoizey.gremlins");
        assert_eq!(v[1].version.as_deref(), Some("0.26.0"));
    }

    #[test]
    fn empty_and_absent_manifests_are_empty_profiles() {
        assert!(parse_manifest("").unwrap().is_empty());
        assert!(parse_manifest("[]").unwrap().is_empty());
        assert!(read_manifest(Path::new("/nonexistent/extensions.json")).unwrap().is_empty());
    }

    #[test]
    fn version_suffix_stripping_keeps_hyphenated_names() {
        assert_eq!(strip_version_suffix("golang.go-0.52.2"), "golang.go");
        assert_eq!(
            strip_version_suffix("ms-vscode.cpptools-extension-pack-1.3.1"),
            "ms-vscode.cpptools-extension-pack"
        );
        // No version suffix at all.
        assert_eq!(strip_version_suffix("ms-vscode.cpptools-themes"), "ms-vscode.cpptools-themes");
    }

    fn inv(profiles: Vec<(&str, Vec<&str>)>, on_disk: Vec<&str>) -> Inventory {
        Inventory {
            profiles: profiles
                .into_iter()
                .map(|(name, ids)| ProfileExtensions {
                    profile: name.to_string(),
                    location: None,
                    extensions: ids
                        .into_iter()
                        .map(|id| InstalledExtension {
                            id: id.to_string(),
                            version: None,
                            relative_location: None,
                        })
                        .collect(),
                })
                .collect(),
            on_disk: on_disk.into_iter().map(String::from).collect(),
            packs: BTreeMap::new(),
            facts: BTreeMap::new(),
        }
    }

    #[test]
    fn default_manifest_is_not_a_superset_of_named_profiles() {
        // Mirrors the real install: golang.go is on disk and in Go, but absent from Default.
        let i = inv(
            vec![("Default", vec!["eamodio.gitlens"]), ("Go", vec!["golang.go"])],
            vec!["eamodio.gitlens", "golang.go"],
        );
        assert_eq!(i.referenced().len(), 2);
        assert!(i.orphans().is_empty());
        assert!(i.dangling().is_empty());
    }

    #[test]
    fn only_undeclared_members_of_declared_packs_are_reported() {
        let mut i = inv(vec![], vec![]);
        i.packs.insert(
            "vscjava.vscode-java-pack".into(),
            vec!["redhat.java".into(), "vscjava.vscode-maven".into()],
        );
        // Real case: a genuine extension that also declares a pack.
        i.packs.insert(
            "ms-python.python".into(),
            vec!["ms-python.vscode-pylance".into(), "ms-python.debugpy".into()],
        );

        // Pack not declared at all -> nothing to pin.
        let declared: BTreeSet<String> = ["redhat.java"].iter().map(|s| s.to_string()).collect();
        assert!(i.unpinned_pack_members(&declared).is_empty());

        // Declared with only some members pinned -> report the rest.
        let declared: BTreeSet<String> = ["ms-python.python", "ms-python.debugpy"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let missing = i.unpinned_pack_members(&declared);
        assert_eq!(missing["ms-python.python"], vec!["ms-python.vscode-pylance"]);

        // Fully pinned -> clean.
        let declared: BTreeSet<String> =
            ["ms-python.python", "ms-python.debugpy", "ms-python.vscode-pylance"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert!(i.unpinned_pack_members(&declared).is_empty());
    }

    #[test]
    fn orphans_and_dangling_are_distinguished() {
        let i = inv(
            vec![("Default", vec!["a.a", "ghost.ghost"])],
            vec!["a.a", "leftover.leftover"],
        );
        assert_eq!(i.orphans().into_iter().collect::<Vec<_>>(), vec!["leftover.leftover"]);
        assert_eq!(i.dangling().into_iter().collect::<Vec<_>>(), vec!["ghost.ghost"]);
    }
}

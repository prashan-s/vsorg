//! The TOML manifest: the single source of truth a profile set is generated from.
//!
//! VS Code profiles do not inherit — "New Profile from Default" is a one-time copy — so a shared
//! base set duplicated by hand into each profile drifts. Here `base` is declared once and unioned
//! into every profile at plan time.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Content kinds that can be inherited from Default, mirroring `storage.json`'s `useDefaultFlags`.
pub const SHARABLE: [&str; 8] = [
    "settings",
    "keybindings",
    "snippets",
    "tasks",
    "extensions",
    "prompts",
    "mcp",
    "languageModels",
];

/// `deny_unknown_fields` guards a TOML footgun: keys written *after* a `[meta]` header belong to
/// that table, so `[meta]\nversion = 1\nbase = [...]` silently defines `meta.base` and the real
/// top-level `base` stays empty — every profile would then quietly lose its shared set. Erroring
/// beats reconciling against a manifest that does not say what the author thinks it says.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub version: u32,
}

impl Default for Meta {
    fn default() -> Self {
        Meta { version: 1 }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Remove extensions present in the profile but absent from the manifest. Off by default so
    /// `apply` is additive and cannot silently destroy an unrecorded setup.
    #[serde(default)]
    pub prune: bool,
    /// Content inherited from Default. Reported as manual UI steps — see [`crate::apply`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared: Vec<String>,
    /// Skip `base` for this profile. For deliberately bare profiles.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_base: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub meta: Meta,
    /// Extensions unioned into every profile that does not set `no_base`.
    #[serde(default)]
    pub base: Vec<String>,
    /// Extensions deliberately left unassigned. Excluded from orphan warnings in `doctor`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileSpec>,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Manifest> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        let m: Manifest = toml::from_str(&text)
            .with_context(|| format!("parsing manifest {}", path.display()))?;
        m.validate()?;
        Ok(m)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// The full desired set for a profile: `base ∪ extensions`, normalised and deduplicated.
    pub fn desired(&self, profile: &str) -> Option<BTreeSet<String>> {
        let spec = self.profiles.get(profile)?;
        let mut set: BTreeSet<String> = spec.extensions.iter().map(|s| normalize(s)).collect();
        if !spec.no_base {
            set.extend(self.base.iter().map(|s| normalize(s)));
        }
        Some(set)
    }

    /// Every extension ID the manifest mentions anywhere.
    pub fn all_ids(&self) -> BTreeSet<String> {
        self.base
            .iter()
            .chain(self.ignore.iter())
            .chain(self.profiles.values().flat_map(|p| p.extensions.iter()))
            .map(|s| normalize(s))
            .collect()
    }

    fn validate(&self) -> Result<()> {
        if self.meta.version != 1 {
            bail!(
                "manifest version {} is not supported by this build (expected 1)",
                self.meta.version
            );
        }
        if self.profiles.is_empty() {
            bail!("manifest declares no profiles");
        }

        for (name, spec) in &self.profiles {
            if name.trim().is_empty() {
                bail!("profile names cannot be blank");
            }
            for flag in &spec.shared {
                if !SHARABLE.contains(&flag.as_str()) {
                    bail!(
                        "profile `{name}`: unknown shared content `{flag}` \
                         (expected one of: {})",
                        SHARABLE.join(", ")
                    );
                }
            }
            // Sharing extensions from Default would make the profile's own extension list inert,
            // defeating the entire point of partitioning.
            if spec.shared.iter().any(|f| f == "extensions") && !spec.extensions.is_empty() {
                bail!(
                    "profile `{name}` marks extensions as shared from Default yet declares its own \
                     — the declared list would never take effect"
                );
            }
            for id in &spec.extensions {
                check_id(id).map_err(|e| anyhow!("profile `{name}`: {e}"))?;
            }
        }
        for id in self.base.iter().chain(self.ignore.iter()) {
            check_id(id)?;
        }
        Ok(())
    }
}

fn check_id(id: &str) -> Result<()> {
    let t = id.trim();
    if t.is_empty() {
        bail!("empty extension id");
    }
    // Marketplace IDs are exactly `publisher.name`; anything else will fail at install time with a
    // far less obvious message.
    if t.matches('.').count() < 1 || t.starts_with('.') || t.ends_with('.') {
        bail!("`{id}` is not a valid extension id (expected `publisher.name`)");
    }
    if t.contains(char::is_whitespace) {
        bail!("`{id}` contains whitespace");
    }
    Ok(())
}

/// Marketplace IDs are case-insensitive; we compare in lowercase throughout.
pub fn normalize(id: &str) -> String {
    id.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    // `base` must precede `[meta]`: anything after a table header belongs to that table.
    const SAMPLE: &str = r#"
        base = ["eamodio.gitlens", "EditorConfig.EditorConfig"]

        [meta]
        version = 1

        [profiles.default]
        extensions = []
        prune = true

        [profiles.ios]
        description = "Swift / iOS"
        extensions = ["sweetpad.sweetpad"]
        shared = ["keybindings", "snippets"]
    "#;

    fn parse(s: &str) -> Result<Manifest> {
        let m: Manifest = toml::from_str(s)?;
        m.validate()?;
        Ok(m)
    }

    #[test]
    fn base_is_unioned_into_every_profile() {
        let m = parse(SAMPLE).unwrap();
        let ios = m.desired("ios").unwrap();
        assert!(ios.contains("sweetpad.sweetpad"));
        assert!(ios.contains("eamodio.gitlens"));
        // Case from the manifest is normalised away.
        assert!(ios.contains("editorconfig.editorconfig"));
        assert_eq!(ios.len(), 3);
        // Default gets base only.
        assert_eq!(m.desired("default").unwrap().len(), 2);
    }

    #[test]
    fn no_base_opts_a_profile_out() {
        let m = parse(
            r#"
            base = ["a.a"]
            [profiles.bare]
            no_base = true
            extensions = ["b.b"]
        "#,
        )
        .unwrap();
        assert_eq!(m.desired("bare").unwrap().into_iter().collect::<Vec<_>>(), vec!["b.b"]);
    }

    #[test]
    fn rejects_ids_that_are_not_publisher_dot_name() {
        assert!(parse("[profiles.x]\nextensions = [\"gitlens\"]").is_err());
        assert!(parse("[profiles.x]\nextensions = [\"a b.c\"]").is_err());
        assert!(parse("[profiles.x]\nextensions = [\".leading\"]").is_err());
    }

    #[test]
    fn rejects_sharing_extensions_alongside_a_declared_list() {
        // This combination silently makes the declared list inert.
        let r = parse(
            r#"
            [profiles.x]
            extensions = ["a.a"]
            shared = ["extensions"]
        "#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn rejects_unknown_shared_flags_and_versions() {
        assert!(parse("[profiles.x]\nshared = [\"colours\"]").is_err());
        assert!(parse("[meta]\nversion = 2\n[profiles.x]").is_err());
        assert!(parse("base = []").is_err(), "a manifest with no profiles is useless");
    }

    #[test]
    fn base_written_under_meta_is_an_error_not_a_silent_no_op() {
        // The likeliest hand-editing mistake: TOML binds these keys to [meta], so `base` would
        // vanish and every profile would lose its shared set without a word.
        let err = parse(
            r#"
            [meta]
            version = 1
            base = ["eamodio.gitlens"]

            [profiles.ios]
            extensions = ["sweetpad.sweetpad"]
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("base"), "error should name the misplaced key: {err}");
    }

    #[test]
    fn round_trips_through_toml() {
        let m = parse(SAMPLE).unwrap();
        let back = parse(&m.to_toml().unwrap()).unwrap();
        assert_eq!(back.desired("ios"), m.desired("ios"));
        assert_eq!(back.profiles["ios"].shared, vec!["keybindings", "snippets"]);
    }
}

//! Diffs the manifest against live state and emits an ordered action list.
//!
//! Pure: it reads no files and runs no commands, so the whole diff is unit-testable and `plan`
//! is guaranteed non-destructive.

use std::collections::BTreeSet;
use std::fmt;

use crate::manifest::Manifest;
use crate::state::{State, DEFAULT_PROFILE_NAME};
use crate::store::Inventory;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Profile named in the manifest has no counterpart in `storage.json`.
    CreateProfile { profile: String },
    Install { profile: String, id: String },
    /// Only emitted for profiles with `prune = true`.
    Uninstall { profile: String, id: String },
    /// Something the `code` CLI cannot do; the user must do it in the UI.
    Manual { profile: String, instruction: String },
}

impl Action {
    pub fn profile(&self) -> &str {
        match self {
            Action::CreateProfile { profile }
            | Action::Install { profile, .. }
            | Action::Uninstall { profile, .. }
            | Action::Manual { profile, .. } => profile,
        }
    }

    /// True for anything that changes state on disk.
    pub fn is_mutating(&self) -> bool {
        !matches!(self, Action::Manual { .. })
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::CreateProfile { profile } => write!(f, "create profile `{profile}`"),
            Action::Install { profile, id } => write!(f, "{profile}: + {id}"),
            Action::Uninstall { profile, id } => write!(f, "{profile}: - {id}"),
            Action::Manual { profile, instruction } => write!(f, "{profile}: (manual) {instruction}"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    /// Profiles that exist in VS Code but are absent from the manifest. Never auto-deleted —
    /// profile deletion is irreversible and takes settings, keybindings, snippets, tasks and UI
    /// state with it.
    pub unmanaged: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    pub fn mutating(&self) -> impl Iterator<Item = &Action> {
        self.actions.iter().filter(|a| a.is_mutating())
    }

    pub fn manual(&self) -> impl Iterator<Item = &Action> {
        self.actions.iter().filter(|a| !a.is_mutating())
    }

    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut create = 0;
        let mut install = 0;
        let mut uninstall = 0;
        let mut manual = 0;
        for a in &self.actions {
            match a {
                Action::CreateProfile { .. } => create += 1,
                Action::Install { .. } => install += 1,
                Action::Uninstall { .. } => uninstall += 1,
                Action::Manual { .. } => manual += 1,
            }
        }
        (create, install, uninstall, manual)
    }
}

/// Build the plan. `only` restricts it to a single profile.
pub fn build(manifest: &Manifest, state: &State, inv: &Inventory, only: Option<&str>) -> Plan {
    let mut creates = Vec::new();
    let mut installs = Vec::new();
    let mut uninstalls = Vec::new();
    let mut manuals = Vec::new();

    for (name, spec) in &manifest.profiles {
        if let Some(want) = only {
            if !name.eq_ignore_ascii_case(want) {
                continue;
            }
        }

        let is_default = name.eq_ignore_ascii_case(DEFAULT_PROFILE_NAME);
        let entry = if is_default { None } else { state.find(name) };
        let exists = is_default || entry.is_some();

        // `code --profile` matches names exactly, so actions must carry the profile's *live*
        // name, not the manifest key. `[profiles.default]` against VS Code's built-in `Default`
        // otherwise yields "Profile 'default' not found" on every single install.
        let profile = if is_default {
            DEFAULT_PROFILE_NAME.to_string()
        } else {
            entry.map(|e| e.name.clone()).unwrap_or_else(|| name.clone())
        };

        if !exists {
            creates.push(Action::CreateProfile { profile: profile.clone() });
        }

        // A profile we are about to create has nothing installed; treat as empty rather than
        // falling back to Default's set, which would under-report installs.
        let actual: BTreeSet<String> = if exists {
            inv.get(name).map(|p| p.ids()).unwrap_or_default()
        } else {
            BTreeSet::new()
        };

        let desired = manifest.desired(name).unwrap_or_default();

        for id in desired.difference(&actual) {
            installs.push(Action::Install { profile: profile.clone(), id: id.clone() });
        }

        if spec.prune {
            for id in actual.difference(&desired) {
                uninstalls.push(Action::Uninstall { profile: profile.clone(), id: id.clone() });
            }
        }

        // `code --profile` creates with all flags false; the toggles live only in the UI.
        for flag in &spec.shared {
            let already = entry.map(|e| e.flags().is_set(flag)).unwrap_or(false);
            if !already {
                manuals.push(Action::Manual {
                    profile: profile.clone(),
                    instruction: format!(
                        "mark `{flag}` as shared with Default \
                         (gear -> Profiles -> {profile} -> Contents)"
                    ),
                });
            }
        }
    }

    // Ordered by kind, not by profile. Profiles must exist before anything installs into them,
    // and every install must land before the prunes: extension binaries are shared, so dropping
    // one from Default before the profile that still wants it has claimed it forces a needless
    // re-download from the marketplace.
    let mut plan = Plan::default();
    plan.actions.extend(creates);
    plan.actions.extend(installs);
    plan.actions.extend(uninstalls);
    plan.actions.extend(manuals);

    if only.is_none() {
        let managed: BTreeSet<String> =
            manifest.profiles.keys().map(|k| k.to_ascii_lowercase()).collect();
        for p in state.manageable() {
            if !managed.contains(&p.name.to_ascii_lowercase()) {
                plan.unmanaged.push(p.name.clone());
            }
        }
        plan.unmanaged.sort();
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{InstalledExtension, ProfileExtensions};
    use std::collections::BTreeMap;

    fn manifest(toml_src: &str) -> Manifest {
        toml::from_str(toml_src).unwrap()
    }

    fn state(profiles: &[(&str, &str)]) -> State {
        let entries: Vec<String> = profiles
            .iter()
            .map(|(name, loc)| format!(r#"{{"location":"{loc}","name":"{name}"}}"#))
            .collect();
        State::parse(&format!(r#"{{"userDataProfiles":[{}]}}"#, entries.join(","))).unwrap()
    }

    fn inventory(profiles: &[(&str, &[&str])]) -> Inventory {
        Inventory {
            profiles: profiles
                .iter()
                .map(|(name, ids)| ProfileExtensions {
                    profile: name.to_string(),
                    location: None,
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

    #[test]
    fn a_manifest_matching_reality_produces_no_actions() {
        let m = manifest(
            r#"base = ["eamodio.gitlens"]
               [profiles.ios]
               extensions = ["sweetpad.sweetpad"]"#,
        );
        let s = state(&[("ios", "-abc")]);
        let i = inventory(&[("ios", &["eamodio.gitlens", "sweetpad.sweetpad"])]);
        assert!(build(&m, &s, &i, None).is_empty());
    }

    #[test]
    fn missing_profile_yields_create_plus_full_install_set() {
        let m = manifest(
            r#"base = ["eamodio.gitlens"]
               [profiles.web]
               extensions = ["esbenp.prettier-vscode"]"#,
        );
        let p = build(&m, &state(&[]), &inventory(&[]), None);
        assert_eq!(p.counts(), (1, 2, 0, 0));
        assert_eq!(p.actions[0], Action::CreateProfile { profile: "web".into() });
    }

    #[test]
    fn apply_is_additive_unless_prune_is_set() {
        let m = manifest(r#"[profiles.web]
                            extensions = ["a.a"]"#);
        let s = state(&[("web", "-w")]);
        let i = inventory(&[("web", &["a.a", "stray.stray"])]);
        assert!(build(&m, &s, &i, None).is_empty(), "extra extension must not be touched");

        let m = manifest(
            r#"[profiles.web]
               extensions = ["a.a"]
               prune = true"#,
        );
        let p = build(&m, &s, &i, None);
        assert_eq!(p.counts(), (0, 0, 1, 0));
        assert_eq!(p.actions[0], Action::Uninstall { profile: "web".into(), id: "stray.stray".into() });
    }

    #[test]
    fn default_is_managed_without_a_storage_entry() {
        // Default has no userDataProfiles record, so it must never be reported as missing.
        let m = manifest(
            r#"base = ["eamodio.gitlens"]
               [profiles.default]
               prune = true"#,
        );
        let i = inventory(&[("Default", &["eamodio.gitlens", "sweetpad.sweetpad"])]);
        let p = build(&m, &state(&[]), &i, None);
        assert_eq!(p.counts(), (0, 0, 1, 0), "prune Default to base, never create it");
    }

    #[test]
    fn actions_carry_the_live_profile_name_not_the_manifest_key() {
        // `code --profile` is an exact match: emitting the lowercase manifest key makes every
        // command fail with "Profile 'default' not found".
        let m = manifest(
            r#"[profiles.default]
               extensions = ["a.a"]
               [profiles."node.js"]
               extensions = ["b.b"]"#,
        );
        let s = state(&[("Node.js", "-n")]);
        let p = build(&m, &s, &inventory(&[("Default", &[]), ("Node.js", &[])]), None);

        let names: BTreeSet<&str> = p.actions.iter().map(|a| a.profile()).collect();
        assert!(names.contains("Default"), "got {names:?}");
        assert!(names.contains("Node.js"), "got {names:?}");
        assert!(!names.contains("default"));
        assert!(!names.contains("node.js"));
    }

    #[test]
    fn a_profile_absent_from_vs_code_keeps_the_manifest_casing() {
        // Nothing to resolve against, so the manifest key becomes the created profile's name.
        let m = manifest(r#"[profiles.ios]
                            extensions = ["a.a"]"#);
        let p = build(&m, &state(&[]), &inventory(&[]), None);
        assert!(p.actions.iter().all(|a| a.profile() == "ios"));
    }

    #[test]
    fn creates_precede_installs_which_precede_uninstalls() {
        // Binaries are shared: pruning Default before the new profile has claimed an extension
        // would force a re-download.
        let m = manifest(
            r#"[profiles.default]
               extensions = []
               prune = true
               [profiles.web]
               extensions = ["shared.ext"]"#,
        );
        let i = inventory(&[("Default", &["shared.ext"])]);
        let p = build(&m, &state(&[]), &i, None);

        let kinds: Vec<u8> = p
            .actions
            .iter()
            .map(|a| match a {
                Action::CreateProfile { .. } => 0,
                Action::Install { .. } => 1,
                Action::Uninstall { .. } => 2,
                Action::Manual { .. } => 3,
            })
            .collect();
        assert!(kinds.windows(2).all(|w| w[0] <= w[1]), "out of order: {kinds:?}");
        assert_eq!(kinds, vec![0, 1, 2]);
    }

    #[test]
    fn shared_flags_become_manual_steps_only_when_not_already_set() {
        let m = manifest(
            r#"[profiles.ios]
               shared = ["keybindings"]"#,
        );
        let unset = state(&[("ios", "-i")]);
        assert_eq!(build(&m, &unset, &inventory(&[("ios", &[])]), None).counts().3, 1);

        let set = State::parse(
            r#"{"userDataProfiles":[{"location":"-i","name":"ios",
                "useDefaultFlags":{"keybindings":true}}]}"#,
        )
        .unwrap();
        assert_eq!(build(&m, &set, &inventory(&[("ios", &[])]), None).counts().3, 0);
    }

    #[test]
    fn existing_profiles_absent_from_the_manifest_are_reported_never_deleted() {
        let m = manifest(r#"[profiles.web]"#);
        let s = state(&[("web", "-w"), ("Go", "-g")]);
        let p = build(&m, &s, &inventory(&[("web", &[]), ("Go", &["golang.go"])]), None);
        assert_eq!(p.unmanaged, vec!["Go"]);
        assert!(p.actions.iter().all(|a| a.profile() == "web"));
    }

    #[test]
    fn only_filter_scopes_the_plan_to_one_profile() {
        let m = manifest(
            r#"base = ["a.a"]
               [profiles.web]
               [profiles.ios]"#,
        );
        let p = build(&m, &state(&[("web", "-w"), ("ios", "-i")]), &inventory(&[]), Some("ios"));
        assert!(p.actions.iter().all(|a| a.profile() == "ios"));
        assert!(p.unmanaged.is_empty(), "scoped runs must not report unmanaged profiles");
    }
}

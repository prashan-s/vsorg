//! Reads `globalStorage/storage.json` — VS Code's registry of profiles and workspace bindings.
//!
//! Only the two keys we care about are modelled; the file holds a great deal more and we must not
//! disturb it. The tool never writes here (see the module docs on [`crate::apply`]).

use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::paths::Layout;

/// Sentinel `profileAssociations` uses for folders bound to the Default profile.
pub const DEFAULT_PROFILE_SENTINEL: &str = "__default__profile__";

/// Display name VS Code shows for the built-in profile, which has no `storage.json` entry.
pub const DEFAULT_PROFILE_NAME: &str = "Default";

/// Per-profile toggles for content inherited from Default. Absent in `storage.json` means every
/// flag is false — the profile owns all of its content.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UseDefaultFlags {
    pub settings: bool,
    pub keybindings: bool,
    pub snippets: bool,
    pub tasks: bool,
    pub extensions: bool,
    pub prompts: bool,
    pub mcp: bool,
    #[serde(rename = "languageModels")]
    pub language_models: bool,
}

impl UseDefaultFlags {
    /// Flag names that are set, in the order VS Code's UI lists them.
    pub fn enabled(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        for (name, on) in [
            ("settings", self.settings),
            ("keybindings", self.keybindings),
            ("snippets", self.snippets),
            ("tasks", self.tasks),
            ("extensions", self.extensions),
            ("prompts", self.prompts),
            ("mcp", self.mcp),
            ("languageModels", self.language_models),
        ] {
            if on {
                v.push(name);
            }
        }
        v
    }

    pub fn is_set(&self, name: &str) -> bool {
        match name {
            "settings" => self.settings,
            "keybindings" => self.keybindings,
            "snippets" => self.snippets,
            "tasks" => self.tasks,
            "extensions" => self.extensions,
            "prompts" => self.prompts,
            "mcp" => self.mcp,
            "languageModels" | "language_models" => self.language_models,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfileEntry {
    /// Directory name under `User/profiles/`, e.g. `-29f7c7e6` or `builtin/agents`.
    pub location: String,
    pub name: String,
    #[serde(rename = "useDefaultFlags")]
    pub use_default_flags: Option<UseDefaultFlags>,
}

impl ProfileEntry {
    /// Built-in profiles (currently `Agents`) ship with VS Code and must not be rewritten.
    pub fn is_builtin(&self) -> bool {
        self.location.starts_with("builtin/")
    }

    pub fn flags(&self) -> UseDefaultFlags {
        self.use_default_flags.clone().unwrap_or_default()
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ProfileAssociations {
    #[serde(default)]
    workspaces: BTreeMap<String, String>,
    #[serde(default)]
    #[allow(dead_code)]
    empty_windows: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawStorage {
    #[serde(rename = "userDataProfiles", default)]
    user_data_profiles: Vec<ProfileEntry>,
    #[serde(rename = "profileAssociations", default)]
    profile_associations: ProfileAssociations,
}

/// The profile registry, with Default synthesised in at index 0 so callers can treat every
/// profile uniformly.
#[derive(Debug, Clone)]
pub struct State {
    /// Named profiles from `storage.json`; does **not** include Default.
    pub profiles: Vec<ProfileEntry>,
    /// `file:///abs/path` → profile `location`, or [`DEFAULT_PROFILE_SENTINEL`].
    pub workspaces: BTreeMap<String, String>,
}

impl State {
    pub fn load(layout: &Layout) -> Result<State> {
        let path = &layout.storage_json;
        // A fresh install has no storage.json until the first profile is created.
        if !path.exists() {
            return Ok(State { profiles: Vec::new(), workspaces: BTreeMap::new() });
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<State> {
        let raw: RawStorage = serde_json::from_str(text)?;
        Ok(State {
            profiles: raw.user_data_profiles,
            workspaces: raw.profile_associations.workspaces,
        })
    }

    /// Case-insensitive lookup by display name. `"Default"` returns `None` — Default has no
    /// `storage.json` entry; callers must special-case it.
    pub fn find(&self, name: &str) -> Option<&ProfileEntry> {
        self.profiles.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    /// Profiles a user may reconcile: named, non-builtin.
    pub fn manageable(&self) -> impl Iterator<Item = &ProfileEntry> {
        self.profiles.iter().filter(|p| !p.is_builtin())
    }

    /// Folders bound to the given profile location, sorted.
    pub fn workspaces_for(&self, location: &str) -> Vec<&str> {
        let mut v: Vec<&str> = self
            .workspaces
            .iter()
            .filter(|(_, loc)| loc.as_str() == location)
            .map(|(ws, _)| ws.as_str())
            .collect();
        v.sort_unstable();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shape captured verbatim from a real 1.133.0 install.
    const SAMPLE: &str = r#"{
      "backupWorkspaces": {},
      "userDataProfiles": [
        { "location": "-29f7c7e6", "name": "Go" },
        { "location": "-529b84bd", "name": "Node.js" },
        { "location": "builtin/agents", "name": "Agents",
          "useDefaultFlags": { "settings": true, "keybindings": true, "prompts": true,
            "mcp": true, "languageModels": true, "snippets": true, "tasks": true,
            "extensions": true } }
      ],
      "profileAssociations": {
        "workspaces": {
          "file:///Users/x/Scripts": "__default__profile__",
          "file:///Users/x/svc": "-29f7c7e6"
        }
      }
    }"#;

    #[test]
    fn parses_profiles_and_associations() {
        let s = State::parse(SAMPLE).unwrap();
        assert_eq!(s.profiles.len(), 3);
        assert_eq!(s.find("go").unwrap().location, "-29f7c7e6");
        assert!(s.find("Default").is_none());
        assert_eq!(s.workspaces.len(), 2);
        assert_eq!(s.workspaces_for("-29f7c7e6"), vec!["file:///Users/x/svc"]);
    }

    #[test]
    fn builtin_profiles_are_excluded_from_management() {
        let s = State::parse(SAMPLE).unwrap();
        let names: Vec<&str> = s.manageable().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Go", "Node.js"]);
    }

    #[test]
    fn missing_use_default_flags_means_nothing_is_shared() {
        let s = State::parse(SAMPLE).unwrap();
        assert!(s.find("Go").unwrap().flags().enabled().is_empty());
        assert_eq!(s.find("Agents").unwrap().flags().enabled().len(), 8);
    }
}

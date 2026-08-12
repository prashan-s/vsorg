//! Resolution of the VS Code directories we read from.
//!
//! Two independent trees matter:
//!
//! * the *user data* dir, holding `globalStorage/storage.json` and `profiles/<loc>/`
//! * the *extensions* dir, holding the physical `<pub>.<name>-<ver>/` folders shared by every
//!   profile, plus `extensions.json` — which is the **Default profile's** extension list, not a
//!   global registry.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

/// Which VS Code build to operate on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Stable,
    Insiders,
    Vscodium,
}

impl Flavor {
    /// Directory name under the platform's application-support root.
    fn user_data_dir_name(self) -> &'static str {
        match self {
            Flavor::Stable => "Code",
            Flavor::Insiders => "Code - Insiders",
            Flavor::Vscodium => "VSCodium",
        }
    }

    /// Directory name under `$HOME` holding the shared extension folders.
    fn extensions_dir_name(self) -> &'static str {
        match self {
            Flavor::Stable => ".vscode",
            Flavor::Insiders => ".vscode-insiders",
            Flavor::Vscodium => ".vscode-oss",
        }
    }

    /// Executable used for all mutating operations.
    pub fn cli_binary(self) -> &'static str {
        match self {
            Flavor::Stable => "code",
            Flavor::Insiders => "code-insiders",
            Flavor::Vscodium => "codium",
        }
    }

    /// Executable file names the *main* process runs as, across platforms. macOS capitalises the
    /// binary inside the bundle; Linux and Windows use the lowercase launcher name.
    pub fn main_binary_names(self) -> &'static [&'static str] {
        match self {
            Flavor::Stable => &["Code", "code", "code.exe", "Code.exe"],
            Flavor::Insiders => {
                &["Code - Insiders", "code-insiders", "code-insiders.exe", "Code - Insiders.exe"]
            }
            Flavor::Vscodium => &["VSCodium", "codium", "codium.exe", "VSCodium.exe"],
        }
    }

    /// Name shown to the user in diagnostics.
    pub fn display_name(self) -> &'static str {
        match self {
            Flavor::Stable => "VS Code",
            Flavor::Insiders => "VS Code Insiders",
            Flavor::Vscodium => "VSCodium",
        }
    }

    pub fn parse(s: &str) -> Result<Flavor> {
        match s.to_ascii_lowercase().as_str() {
            "stable" | "code" => Ok(Flavor::Stable),
            "insiders" => Ok(Flavor::Insiders),
            "vscodium" | "codium" | "oss" => Ok(Flavor::Vscodium),
            other => Err(anyhow!(
                "unknown flavor `{other}` (expected stable, insiders, or vscodium)"
            )),
        }
    }
}

/// Every path the tool touches, resolved once at startup.
#[derive(Debug, Clone)]
pub struct Layout {
    pub flavor: Flavor,
    /// `.../Code/User`
    pub user_dir: PathBuf,
    /// `.../Code/User/globalStorage/storage.json`
    pub storage_json: PathBuf,
    /// `.../Code/User/profiles`
    pub profiles_dir: PathBuf,
    /// `~/.vscode/extensions`
    pub extensions_dir: PathBuf,
}

impl Layout {
    /// Resolve from the real environment. `override_user_dir` mirrors `code --user-data-dir`.
    pub fn discover(flavor: Flavor, override_user_dir: Option<&Path>) -> Result<Layout> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine home directory"))?;

        let user_dir = match override_user_dir {
            Some(p) => p.join("User"),
            None => platform_app_support(&home)?.join(flavor.user_data_dir_name()).join("User"),
        };

        let extensions_dir = home.join(flavor.extensions_dir_name()).join("extensions");

        Ok(Layout::from_roots(flavor, user_dir, extensions_dir))
    }

    /// Build a layout from explicit roots. Used by tests against fixture trees.
    pub fn from_roots(flavor: Flavor, user_dir: PathBuf, extensions_dir: PathBuf) -> Layout {
        Layout {
            flavor,
            storage_json: user_dir.join("globalStorage").join("storage.json"),
            profiles_dir: user_dir.join("profiles"),
            user_dir,
            extensions_dir,
        }
    }

    /// The Default profile's extension manifest. Note the asymmetry: every other profile keeps
    /// its list under `User/profiles/<loc>/extensions.json`, but Default's lives beside the
    /// physical extension folders.
    pub fn default_profile_extensions_json(&self) -> PathBuf {
        self.extensions_dir.join("extensions.json")
    }

    /// A named profile's extension manifest, given its `location` from `storage.json`.
    pub fn profile_extensions_json(&self, location: &str) -> PathBuf {
        self.profiles_dir.join(location).join("extensions.json")
    }

    pub fn profile_dir(&self, location: &str) -> PathBuf {
        self.profiles_dir.join(location)
    }

    /// Fails early with an actionable message rather than surfacing a bare ENOENT later.
    pub fn validate(&self) -> Result<()> {
        if !self.user_dir.is_dir() {
            return Err(anyhow!(
                "VS Code user directory not found at {}\n\
                 hint: pass --user-data-dir, or --flavor insiders/vscodium",
                self.user_dir.display()
            ));
        }
        if !self.extensions_dir.is_dir() {
            return Err(anyhow!(
                "VS Code extensions directory not found at {}",
                self.extensions_dir.display()
            ));
        }
        Ok(())
    }
}

fn platform_app_support(home: &Path) -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Ok(home.join("Library").join("Application Support"))
    }
    #[cfg(target_os = "windows")]
    {
        let _ = home;
        dirs::config_dir().ok_or_else(|| anyhow!("cannot determine %APPDATA%"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Ok(dirs::config_dir().unwrap_or_else(|| home.join(".config")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_manifest_sits_beside_the_binaries() {
        let l = Layout::from_roots(
            Flavor::Stable,
            PathBuf::from("/u/User"),
            PathBuf::from("/u/.vscode/extensions"),
        );
        assert_eq!(
            l.default_profile_extensions_json(),
            PathBuf::from("/u/.vscode/extensions/extensions.json")
        );
        assert_eq!(
            l.profile_extensions_json("-29f7c7e6"),
            PathBuf::from("/u/User/profiles/-29f7c7e6/extensions.json")
        );
    }

    #[test]
    fn flavor_parsing_accepts_aliases() {
        assert_eq!(Flavor::parse("codium").unwrap(), Flavor::Vscodium);
        assert_eq!(Flavor::parse("STABLE").unwrap(), Flavor::Stable);
        assert!(Flavor::parse("emacs").is_err());
    }
}

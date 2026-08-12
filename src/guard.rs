//! Detects a running VS Code.
//!
//! VS Code holds profile state in memory and rewrites `storage.json` on exit, so a profile created
//! while it runs can be silently reverted. Extension installs go through its own CLI and are safe
//! either way; only profile creation needs the guard.

use std::path::Path;

use sysinfo::System;

use crate::paths::Flavor;

/// Path segments that only ever appear in Electron helper processes. The renderers, GPU process
/// and crash handler all live under these, and all share the app bundle path with the main
/// process — so the bundle path alone cannot distinguish them.
const HELPER_SEGMENTS: [&str; 3] = ["Frameworks/", "Helpers/", "Contents/Resources/"];

/// True if a *main* VS Code process for this flavor is running.
pub fn is_running(flavor: Flavor) -> bool {
    let mut sys = System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    sys.processes().values().any(|p| {
        let Some(exe) = p.exe() else { return false };
        let cmd: Vec<String> = p.cmd().iter().map(|a| a.to_string_lossy().to_string()).collect();
        is_main_process(flavor, exe, &cmd)
    })
}

/// Pure predicate, split out so the classification is testable without spawning anything.
///
/// `cmd` is frequently empty: macOS refuses process arguments for processes the caller does not
/// own, so the `--type=` test can only ever *add* confidence, never be relied upon.
pub fn is_main_process(flavor: Flavor, exe: &Path, cmd: &[String]) -> bool {
    let Some(file) = exe.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if !flavor.main_binary_names().contains(&file) {
        return false;
    }

    let path = exe.to_string_lossy().replace('\\', "/");
    if HELPER_SEGMENTS.iter().any(|seg| path.contains(seg)) {
        return false;
    }

    // Every Electron helper carries `--type=`; the main process never does.
    !cmd.iter().skip(1).any(|a| a.starts_with("--type="))
}

/// Human-readable reason used in error messages.
pub fn running_message(flavor: Flavor) -> String {
    format!(
        "{} is running.\n\
         A profile created while the editor is open can be reverted when it exits, because VS Code \
         rewrites storage.json on quit.\n\
         Quit it fully (Cmd-Q, not just closing the windows), or pass --force-running to proceed \
         with extension changes only.",
        flavor.display_name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn recognises_the_macos_main_process() {
        assert!(is_main_process(
            Flavor::Stable,
            &p("/Applications/Visual Studio Code.app/Contents/MacOS/Code"),
            &[]
        ));
    }

    #[test]
    fn rejects_helpers_that_share_the_bundle_path() {
        // Observed on a machine with VS Code fully quit: these outlive the main process, so
        // matching on the bundle path alone would report a running editor forever.
        assert!(!is_main_process(
            Flavor::Stable,
            &p("/Applications/Visual Studio Code.app/Contents/Frameworks/Electron Framework.framework/Helpers/chrome_crashpad_handler"),
            &[]
        ));
        assert!(!is_main_process(
            Flavor::Stable,
            &p("/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper (Renderer).app/Contents/MacOS/Code Helper (Renderer)"),
            &[]
        ));
    }

    #[test]
    fn rejects_a_renderer_that_reuses_the_main_binary_name() {
        // The Linux layout: helpers are the same binary distinguished only by --type=.
        assert!(is_main_process(Flavor::Stable, &p("/usr/share/code/code"), &["code".into()]));
        assert!(!is_main_process(
            Flavor::Stable,
            &p("/usr/share/code/code"),
            &["code".into(), "--type=renderer".into()]
        ));
    }

    #[test]
    fn flavors_do_not_match_each_others_processes() {
        let insiders = p("/Applications/Visual Studio Code - Insiders.app/Contents/MacOS/Code - Insiders");
        assert!(is_main_process(Flavor::Insiders, &insiders, &[]));
        assert!(!is_main_process(Flavor::Stable, &insiders, &[]));

        let stable = p("/Applications/Visual Studio Code.app/Contents/MacOS/Code");
        assert!(!is_main_process(Flavor::Vscodium, &stable, &[]));
    }
}

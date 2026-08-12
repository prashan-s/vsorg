//! Executes a [`Plan`] through the `code` CLI.
//!
//! Reads go straight to the JSON on disk, but every write goes through the editor's own CLI. The
//! on-disk schema shifts between VS Code releases and the editor rewrites `storage.json` on exit,
//! so hand-rolled writes are both version-fragile and liable to be clobbered.
//!
//! Two things the CLI cannot do, surfaced as [`Action::Manual`] rather than forced:
//!
//! * `useDefaultFlags` — the per-profile shared-content toggles, UI-only.
//! * `profileAssociations` — VS Code owns folder bindings; `vsorg bind` emits the command that
//!   makes the editor persist one itself.

use std::process::Command;

use anyhow::{bail, Context, Result};
use owo_colors::OwoColorize;

use crate::paths::Flavor;
use crate::plan::{Action, Plan};

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub dry_run: bool,
    /// Continue past a failing install instead of aborting. Useful for a first run where a
    /// marketplace ID has gone stale.
    pub keep_going: bool,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub succeeded: usize,
    pub failed: Vec<(Action, String)>,
}

/// Run every mutating action. Manual actions are returned to the caller to print.
pub fn execute(plan: &Plan, flavor: Flavor, opts: Options) -> Result<Outcome> {
    let mut outcome = Outcome::default();

    for action in plan.mutating() {
        let argv = argv_for(action, flavor);
        let rendered = format!("{} {}", flavor.cli_binary(), argv.join(" "));

        if opts.dry_run {
            println!("  {} {}", "would run".dimmed(), rendered);
            outcome.succeeded += 1;
            continue;
        }

        println!("  {} {}", "→".dimmed(), rendered.dimmed());
        match run(flavor, &argv) {
            Ok(()) => outcome.succeeded += 1,
            Err(e) => {
                let msg = e.to_string();
                eprintln!("  {} {action}: {msg}", "failed".red());
                outcome.failed.push((action.clone(), msg));
                if !opts.keep_going {
                    bail!("aborting after a failed action; re-run with --keep-going to continue");
                }
            }
        }
    }

    Ok(outcome)
}

/// The exact `code` invocation for an action. Split out so `--dry-run` prints precisely what a
/// real run would execute.
pub fn argv_for(action: &Action, _flavor: Flavor) -> Vec<String> {
    match action {
        // `code --profile <name>` creates the profile if it does not exist. `--wait` is omitted
        // deliberately: without a folder argument the CLI returns immediately.
        Action::CreateProfile { profile } => {
            vec!["--profile".into(), profile.clone()]
        }
        Action::Install { profile, id } => vec![
            "--install-extension".into(),
            id.clone(),
            "--profile".into(),
            profile.clone(),
            "--force".into(),
        ],
        Action::Uninstall { profile, id } => vec![
            "--uninstall-extension".into(),
            id.clone(),
            "--profile".into(),
            profile.clone(),
            "--force".into(),
        ],
        Action::Manual { .. } => Vec::new(),
    }
}

fn run(flavor: Flavor, args: &[String]) -> Result<()> {
    let out = Command::new(flavor.cli_binary())
        .args(args)
        .output()
        .with_context(|| {
            format!(
                "running `{}` — is it on PATH? \
                 (VS Code: Command Palette -> Shell Command: Install 'code' command in PATH)",
                flavor.cli_binary()
            )
        })?;

    if out.status.success() {
        return Ok(());
    }

    // VS Code writes install errors to stdout, not stderr.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let detail: String = stderr
        .lines()
        .chain(stdout.lines())
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("no output")
        .to_string();
    bail!("exit {}: {detail}", out.status.code().unwrap_or(-1))
}

/// Emit the command that makes VS Code persist a workspace → profile binding. Not executed:
/// `code --profile <name> <folder>` opens a window, which should be the user's choice.
pub fn bind_command(flavor: Flavor, folder: &str, profile: &str) -> String {
    format!("{} --profile {} {}", flavor.cli_binary(), shell_quote(profile), shell_quote(folder))
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || "._-/".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_and_uninstall_carry_the_profile_and_force_flags() {
        let a = Action::Install { profile: "web".into(), id: "esbenp.prettier-vscode".into() };
        assert_eq!(
            argv_for(&a, Flavor::Stable),
            vec!["--install-extension", "esbenp.prettier-vscode", "--profile", "web", "--force"]
        );
        let u = Action::Uninstall { profile: "Default".into(), id: "golang.go".into() };
        assert_eq!(
            argv_for(&u, Flavor::Stable),
            vec!["--uninstall-extension", "golang.go", "--profile", "Default", "--force"]
        );
    }

    #[test]
    fn manual_actions_have_no_command() {
        let m = Action::Manual { profile: "ios".into(), instruction: "toggle".into() };
        assert!(argv_for(&m, Flavor::Stable).is_empty());
    }

    #[test]
    fn quotes_profile_names_and_paths_containing_spaces() {
        assert_eq!(
            bind_command(Flavor::Stable, "/Users/x/My Project", "Node.js"),
            "code --profile Node.js '/Users/x/My Project'"
        );
        assert_eq!(
            bind_command(Flavor::Stable, "/tmp/a", "Data Science"),
            "code --profile 'Data Science' /tmp/a"
        );
    }
}

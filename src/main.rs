//! `vsorg` — declarative VS Code profile manager.
//!
//! VS Code profiles do not inherit: "New Profile from Default" copies once, so a shared base set
//! duplicated by hand into every profile drifts. This tool treats profiles as build artifacts
//! generated from a TOML manifest, diffing desired against live state and reconciling through the
//! editor's own CLI.

use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use owo_colors::OwoColorize;

use vscode_organizer::manifest::Manifest;
use vscode_organizer::paths::{Flavor, Layout};
use vscode_organizer::state::{State, DEFAULT_PROFILE_NAME, DEFAULT_PROFILE_SENTINEL};
use vscode_organizer::store::Inventory;
use vscode_organizer::{apply, backup, classify, export, guard, plan, restore};

#[derive(Parser)]
#[command(
    name = "vsorg",
    version,
    about = "Declarative VS Code profile manager",
    long_about = "Generates VS Code profiles from a TOML manifest.\n\n\
                  Reads profile state directly from disk; performs every write through the `code` \
                  CLI. `inventory`, `export`, `init`, `plan`, `bind` and `doctor` never mutate \
                  anything."
)]
struct Cli {
    /// Which VS Code build to operate on: stable, insiders, vscodium
    #[arg(long, global = true, default_value = "stable")]
    flavor: String,

    /// Override the user-data directory (mirrors `code --user-data-dir`)
    #[arg(long, global = true, value_name = "DIR")]
    user_data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show profiles, extension counts, and store health. Read-only.
    Inventory {
        /// Emit JSON instead of a table
        #[arg(long)]
        json: bool,
        /// List every extension id per profile
        #[arg(long)]
        verbose: bool,
    },

    /// Copy every profile's extension list and content files into a directory. Run this before
    /// deleting any profile — deletion is irreversible.
    Export {
        /// Destination directory
        dest: PathBuf,
    },

    /// Write a manifest describing the current install. `plan` against it is a no-op.
    Init {
        /// Output path (default: vscode-organizer.toml)
        output: Option<PathBuf>,
        /// Overwrite an existing file
        #[arg(long)]
        force: bool,
    },

    /// Propose a stack-shaped partition by piping extension metadata through an LLM CLI.
    ///
    /// Reads only; the model returns JSON, which is validated against what is actually installed
    /// before any manifest is written. Use --print-prompt / --from-json to drive the pipe yourself.
    Classify {
        /// Write the manifest here (default: stdout)
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// Command to pipe the prompt through; must read stdin and write stdout.
        /// Defaults to $VSORG_LLM, else the first of claude/llm/codex/ollama on PATH.
        #[arg(long, value_name = "CMD")]
        llm: Option<String>,
        /// Print the prompt and exit, for piping into an LLM yourself
        #[arg(long, conflicts_with_all = ["llm", "from_json"])]
        print_prompt: bool,
        /// Read a previously captured LLM response instead of calling one ("-" for stdin)
        #[arg(long, value_name = "FILE")]
        from_json: Option<PathBuf>,
        /// Upper bound on proposed profiles, excluding Default
        #[arg(long, default_value_t = 5)]
        max_profiles: usize,
        /// Force these profile names instead of letting the model propose them
        #[arg(long, value_name = "NAMES", value_delimiter = ',')]
        profiles: Vec<String>,
        /// Content each generated profile inherits from Default
        #[arg(long, value_name = "KINDS", value_delimiter = ',',
              default_value = "keybindings,snippets")]
        shared: Vec<String>,
        /// Route extensions the model forgot to `ignore` instead of failing
        #[arg(long)]
        allow_unassigned: bool,
        /// Emit `prune = false`, making the generated manifest additive only
        #[arg(long)]
        no_prune: bool,
        /// Seconds to wait for the LLM
        #[arg(long, default_value_t = 180)]
        timeout: u64,
        /// Overwrite an existing output file
        #[arg(long)]
        force: bool,
    },

    /// Diff a manifest against live state. Exits 1 when there is drift.
    Plan {
        #[arg(short, long, value_name = "FILE")]
        manifest: PathBuf,
        /// Restrict to a single profile
        #[arg(short, long)]
        profile: Option<String>,
    },

    /// Reconcile live state with a manifest.
    Apply {
        #[arg(short, long, value_name = "FILE")]
        manifest: PathBuf,
        /// Restrict to a single profile
        #[arg(short, long)]
        profile: Option<String>,
        /// Print the exact `code` commands without running them
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
        /// Proceed even though VS Code is running (extension changes only)
        #[arg(long)]
        force_running: bool,
        /// Do not snapshot the user directory first
        #[arg(long)]
        no_backup: bool,
        /// Where to write the snapshot
        #[arg(long, value_name = "DIR", default_value = "./vsorg-backups")]
        backup_dir: PathBuf,
        /// Continue after a failed action
        #[arg(long)]
        keep_going: bool,
    },

    /// Snapshot the user directory to a timestamped archive.
    ///
    /// `apply` and `restore` do this for you; run it directly before making changes by hand.
    Backup {
        /// Where to write the archive
        #[arg(value_name = "DIR", default_value = "./vsorg-backups")]
        dest: PathBuf,
    },

    /// Restore a snapshot written by `backup`, `apply`, or `restore`.
    ///
    /// Brings back storage.json, profile directories, settings, keybindings and snippets.
    /// Extension binaries are not in the archive — VS Code or `apply` refetches those.
    Restore {
        /// Archive to restore. Omit to use the newest found in ./vsorg-backups, then ./
        archive: Option<PathBuf>,
        /// Look here instead of autodetecting
        #[arg(long, value_name = "DIR", conflicts_with = "archive")]
        backup_dir: Option<PathBuf>,
        /// List what the archive would restore, then exit
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
        /// Do not snapshot the current state before overwriting it
        #[arg(long)]
        no_backup: bool,
        /// Proceed even though VS Code is running
        #[arg(long)]
        force_running: bool,
    },

    /// Print the command that binds a folder to a profile.
    Bind {
        /// Folder to bind
        path: PathBuf,
        /// Profile name
        profile: String,
    },

    /// Report packs, orphans, dangling entries and other footguns.
    Doctor {
        /// Cross-check a manifest against the install as well
        #[arg(short, long, value_name = "FILE")]
        manifest: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{} {e:#}", "error:".red().bold());
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let flavor = Flavor::parse(&cli.flavor)?;
    let layout = Layout::discover(flavor, cli.user_data_dir.as_deref())?;

    match cli.command {
        Cmd::Inventory { json, verbose } => {
            layout.validate()?;
            cmd_inventory(&layout, json, verbose)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Export { dest } => {
            layout.validate()?;
            cmd_export(&layout, &dest)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Init { output, force } => {
            layout.validate()?;
            cmd_init(&layout, output, force)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Classify {
            output,
            llm,
            print_prompt,
            from_json,
            max_profiles,
            profiles,
            shared,
            allow_unassigned,
            no_prune,
            timeout,
            force,
        } => {
            layout.validate()?;
            cmd_classify(
                &layout,
                ClassifyOpts {
                    output,
                    llm,
                    print_prompt,
                    from_json,
                    force,
                    timeout,
                    opts: classify::Options {
                        max_profiles,
                        seed_profiles: profiles,
                        allow_unassigned,
                        prune: !no_prune,
                        shared,
                    },
                },
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Plan { manifest, profile } => {
            layout.validate()?;
            cmd_plan(&layout, &manifest, profile.as_deref())
        }
        Cmd::Apply {
            manifest,
            profile,
            dry_run,
            yes,
            force_running,
            no_backup,
            backup_dir,
            keep_going,
        } => {
            layout.validate()?;
            cmd_apply(
                &layout,
                &manifest,
                profile.as_deref(),
                ApplyOpts { dry_run, yes, force_running, no_backup, backup_dir, keep_going },
            )
        }
        Cmd::Backup { dest } => {
            layout.validate()?;
            let archive = backup::snapshot(&layout.user_dir, &dest)?;
            let bytes = std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
            println!("{} {}", "backup:".green().bold(), archive.display());
            println!("  {} KiB", (bytes / 1024).max(1));
            println!(
                "  {} extension binaries are not included — they live in {} and are shared \
                 across profiles.",
                "note:".yellow(),
                layout.extensions_dir.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Restore { archive, backup_dir, dry_run, yes, no_backup, force_running } => {
            layout.validate()?;
            cmd_restore(
                &layout,
                RestoreOpts { archive, backup_dir, dry_run, yes, no_backup, force_running },
            )
        }
        Cmd::Bind { path, profile } => {
            layout.validate()?;
            cmd_bind(&layout, &path, &profile)
        }
        Cmd::Doctor { manifest } => {
            layout.validate()?;
            cmd_doctor(&layout, manifest.as_deref())
        }
    }
}

fn load(layout: &Layout) -> Result<(State, Inventory)> {
    let state = State::load(layout)?;
    let inv = Inventory::load(layout, &state)?;
    Ok((state, inv))
}

fn cmd_inventory(layout: &Layout, json: bool, verbose: bool) -> Result<()> {
    let (state, inv) = load(layout)?;

    if json {
        let profiles: Vec<serde_json::Value> = inv
            .profiles
            .iter()
            .map(|p| {
                serde_json::json!({
                    "profile": p.profile,
                    "location": p.location,
                    "count": p.extensions.len(),
                    "extensions": p.extensions.iter().map(|e| &e.id).collect::<Vec<_>>(),
                })
            })
            .collect();
        let doc = serde_json::json!({
            "flavor": layout.flavor.cli_binary(),
            "userDir": layout.user_dir,
            "profiles": profiles,
            "onDisk": inv.on_disk.len(),
            "referenced": inv.referenced().len(),
            "orphans": inv.orphans(),
            "dangling": inv.dangling(),
            "packs": inv.packs,
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(());
    }

    println!("{}", layout.user_dir.display().to_string().dimmed());
    println!();

    // Label first, then measure — "(builtin)" and "(inherits Default)" widen the column.
    let rows: Vec<(String, usize, usize)> = inv
        .profiles
        .iter()
        .map(|p| {
            let bound = match &p.location {
                Some(loc) => state.workspaces_for(loc).len(),
                None => state.workspaces_for(DEFAULT_PROFILE_SENTINEL).len(),
            };
            let mut label = p.profile.clone();
            if p.location.as_deref().is_some_and(|l| l.starts_with("builtin/")) {
                label.push_str(" (builtin)");
            }
            // A profile sharing extensions with Default shows Default's set, not its own —
            // without saying so the count reads as duplication.
            if state.find(&p.profile).is_some_and(|e| e.flags().extensions) {
                label.push_str(" (inherits Default)");
            }
            (label, p.extensions.len(), bound)
        })
        .collect();

    let width = rows.iter().map(|(l, _, _)| l.len()).max().unwrap_or(7).max(7);
    println!("  {:<width$}  {:>5}  {}", "PROFILE".bold(), "EXT".bold(), "BOUND".bold());

    for ((label, count, bound), p) in rows.iter().zip(&inv.profiles) {
        println!("  {label:<width$}  {count:>5}  {bound}");
        if verbose {
            for e in &p.extensions {
                println!("      {}", e.id.dimmed());
            }
        }
    }

    println!();
    println!(
        "  {} on disk, {} referenced by a profile",
        inv.on_disk.len().bold(),
        inv.referenced().len().bold()
    );

    let orphans = inv.orphans();
    if !orphans.is_empty() {
        println!(
            "  {} {} on disk but in no profile: {}",
            "!".yellow(),
            orphans.len(),
            join_ids(&orphans)
        );
    }
    let dangling = inv.dangling();
    if !dangling.is_empty() {
        println!(
            "  {} {} referenced but missing from disk: {}",
            "!".yellow(),
            dangling.len(),
            join_ids(&dangling)
        );
    }

    Ok(())
}

fn cmd_export(layout: &Layout, dest: &Path) -> Result<()> {
    let (state, inv) = load(layout)?;
    let summary = export::export(layout, &state, &inv, dest)?;

    println!("exported to {}", dest.display().bold());
    for (name, count) in &summary {
        println!("  {name}: {count} extensions");
    }
    println!("  workspace-bindings.tsv: {} folders", state.workspaces.len());
    println!();
    println!(
        "{} UI state (open editors, layout) is not exportable via the CLI.",
        "note:".yellow()
    );
    println!(
        "      For profiles you intend to delete, also run Command Palette -> \
         `Profiles: Export Profile`."
    );
    Ok(())
}

fn cmd_init(layout: &Layout, output: Option<PathBuf>, force: bool) -> Result<()> {
    let (state, inv) = load(layout)?;
    let path = output.unwrap_or_else(export::default_manifest_path);

    if path.exists() && !force {
        bail!("{} already exists (pass --force to overwrite)", path.display());
    }

    let m = export::derive_manifest(&state, &inv);
    let header = format!(
        "# Generated by vsorg init from {}\n\
         # `base` is the intersection across profiles; everything else is profile-specific.\n\
         # `plan` against this file should report no actions.\n\n",
        layout.user_dir.display()
    );
    std::fs::write(&path, header + &m.to_toml()?)
        .with_context(|| format!("writing {}", path.display()))?;

    println!(
        "wrote {} — {} profiles, {} shared base extensions",
        path.display().bold(),
        m.profiles.len(),
        m.base.len()
    );
    Ok(())
}

struct ClassifyOpts {
    output: Option<PathBuf>,
    llm: Option<String>,
    print_prompt: bool,
    from_json: Option<PathBuf>,
    force: bool,
    timeout: u64,
    opts: classify::Options,
}

fn cmd_classify(layout: &Layout, c: ClassifyOpts) -> Result<()> {
    let (_, inv) = load(layout)?;

    if inv.on_disk.is_empty() {
        bail!("no extensions installed — nothing to classify");
    }

    let prompt = classify::build_prompt(&inv, &c.opts);

    // Half a pipeline: hand the prompt over and let the user run their own model.
    if c.print_prompt {
        print!("{prompt}");
        return Ok(());
    }

    if let Some(path) = &c.output {
        if path.exists() && !c.force {
            bail!("{} already exists (pass --force to overwrite)", path.display());
        }
    }

    let response = match &c.from_json {
        // The other half: ingest a response produced out-of-band.
        Some(path) if path.as_os_str() == "-" => {
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf).context("reading response from stdin")?;
            buf
        }
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?,
        None => {
            let cmd = match &c.llm {
                Some(cmd) => cmd.clone(),
                None => classify::detect_command()?,
            };
            eprintln!(
                "{} {} extensions through `{cmd}`…",
                "classifying".bold(),
                inv.on_disk.len()
            );
            classify::run_llm(&cmd, &prompt, std::time::Duration::from_secs(c.timeout))?
        }
    };

    let (manifest, report) = classify::ingest(&response, &inv, &c.opts)?;

    for line in &report.split_groups {
        eprintln!("  {} {line}", "!".yellow());
    }
    if !report.split_groups.is_empty() {
        eprintln!(
            "  {} split edges break activation (dependencies) or fight prune (packs); \
             move them into one profile or into `base`.",
            "note:".yellow()
        );
    }
    if !report.unassigned.is_empty() {
        eprintln!(
            "  {} {} extension(s) left unassigned, routed to `ignore`",
            "!".yellow(),
            report.unassigned.len()
        );
    }

    let header = format!(
        "# Proposed by `vsorg classify` from {} installed extensions.\n\
         # A suggestion, not a verdict — read it before applying, then:\n\
         #   vsorg doctor -m <this file> && vsorg plan -m <this file>\n\n",
        inv.on_disk.len()
    );
    let body = header + &manifest.to_toml()?;

    match &c.output {
        Some(path) => {
            std::fs::write(path, &body)
                .with_context(|| format!("writing {}", path.display()))?;
            eprintln!(
                "\nwrote {} — {} profiles, {} shared base extensions",
                path.display().bold(),
                manifest.profiles.len(),
                manifest.base.len()
            );
        }
        None => print!("{body}"),
    }

    Ok(())
}

fn cmd_plan(layout: &Layout, manifest_path: &Path, only: Option<&str>) -> Result<ExitCode> {
    let m = Manifest::load(manifest_path)?;
    let (state, inv) = load(layout)?;
    let p = plan::build(&m, &state, &inv, only);

    print_plan(&p);

    Ok(if p.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

fn print_plan(p: &plan::Plan) {
    if p.is_empty() {
        println!("{} live state matches the manifest.", "in sync:".green().bold());
    } else {
        let mut current = String::new();
        for a in &p.actions {
            if a.profile() != current {
                current = a.profile().to_string();
                println!("\n{}", current.bold());
            }
            match a {
                plan::Action::CreateProfile { .. } => {
                    println!("  {} create profile", "*".cyan())
                }
                plan::Action::Install { id, .. } => println!("  {} {id}", "+".green()),
                plan::Action::Uninstall { id, .. } => println!("  {} {id}", "-".red()),
                plan::Action::Manual { instruction, .. } => {
                    println!("  {} {instruction}", "@".yellow())
                }
            }
        }
        let (c, i, u, mn) = p.counts();
        println!(
            "\n{} {c} profile(s) to create, {i} to install, {u} to uninstall, {mn} manual step(s)",
            "summary:".bold()
        );
    }

    if !p.unmanaged.is_empty() {
        println!();
        println!(
            "{} {} not in the manifest and left untouched: {}",
            "note:".yellow(),
            p.unmanaged.len(),
            p.unmanaged.join(", ")
        );
        println!("      Profile deletion is irreversible; do it via Command Palette -> `Profiles: Delete Profile`.");
    }
}

struct ApplyOpts {
    dry_run: bool,
    yes: bool,
    force_running: bool,
    no_backup: bool,
    backup_dir: PathBuf,
    keep_going: bool,
}

fn cmd_apply(
    layout: &Layout,
    manifest_path: &Path,
    only: Option<&str>,
    opts: ApplyOpts,
) -> Result<ExitCode> {
    let m = Manifest::load(manifest_path)?;
    let (state, inv) = load(layout)?;
    let p = plan::build(&m, &state, &inv, only);

    print_plan(&p);

    if p.mutating().next().is_none() {
        print_manual(&p);
        return Ok(ExitCode::SUCCESS);
    }

    // Creating a profile while the editor is open can be reverted when it rewrites storage.json
    // on quit. Extension changes go through its own CLI and are safe.
    let creates = p
        .actions
        .iter()
        .any(|a| matches!(a, plan::Action::CreateProfile { .. }));
    if !opts.dry_run && creates && guard::is_running(layout.flavor) && !opts.force_running {
        bail!(guard::running_message(layout.flavor));
    }

    if !opts.dry_run && !opts.yes && !confirm(&p)? {
        println!("aborted.");
        return Ok(ExitCode::from(2));
    }

    if !opts.dry_run && !opts.no_backup {
        let archive = backup::snapshot(&layout.user_dir, &opts.backup_dir)?;
        println!("\nbackup: {}", archive.display().dimmed());
    }

    println!();
    let outcome = apply::execute(
        &p,
        layout.flavor,
        apply::Options { dry_run: opts.dry_run, keep_going: opts.keep_going },
    )?;

    println!();
    if opts.dry_run {
        println!("{} {} command(s) not run", "dry run:".bold(), outcome.succeeded);
    } else {
        println!("{} {} action(s)", "done:".green().bold(), outcome.succeeded);
    }

    if !outcome.failed.is_empty() {
        println!("{} {} action(s):", "failed:".red().bold(), outcome.failed.len());
        for (a, e) in &outcome.failed {
            println!("  {a}: {e}");
        }
    }

    print_manual(&p);

    if !opts.dry_run {
        println!(
            "\n{} restart VS Code for extension-host changes to take effect.",
            "next:".bold()
        );
    }

    Ok(if outcome.failed.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

fn print_manual(p: &plan::Plan) {
    let manual: Vec<&plan::Action> = p.manual().collect();
    if manual.is_empty() {
        return;
    }
    println!();
    println!("{} the CLI cannot set these — do them in the UI:", "manual:".yellow().bold());
    for a in manual {
        if let plan::Action::Manual { profile, instruction } = a {
            println!("  {profile}: {instruction}");
        }
    }
}

fn confirm(p: &plan::Plan) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("refusing to apply without confirmation on a non-interactive stdin; pass --yes");
    }
    let (c, i, u, _) = p.counts();
    print!("\napply {c} create / {i} install / {u} uninstall? [y/N] ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

struct RestoreOpts {
    archive: Option<PathBuf>,
    backup_dir: Option<PathBuf>,
    dry_run: bool,
    yes: bool,
    no_backup: bool,
    force_running: bool,
}

fn cmd_restore(layout: &Layout, o: RestoreOpts) -> Result<ExitCode> {
    // Where the pre-restore snapshot goes: alongside whatever we restored from, so a chain of
    // restores stays in one place rather than scattering across working directories.
    let mut snapshot_dir = o.backup_dir.clone().unwrap_or_else(|| PathBuf::from("./vsorg-backups"));

    let archive = match &o.archive {
        Some(p) => p.clone(),
        None => {
            let (found, dir) = restore::discover(o.backup_dir.as_deref())?;
            if o.backup_dir.is_none() {
                println!("{} newest snapshot in {}", "found:".dimmed(), dir.display());
            }
            snapshot_dir = dir;
            found
        }
    };

    let entries = restore::inspect(&archive)?;
    let total: u64 = entries.iter().map(|e| e.size).sum();

    println!("{}", archive.display().to_string().dimmed());
    println!(
        "  {} file(s), {} KiB, into {}",
        entries.len().bold(),
        (total / 1024).max(1),
        layout.user_dir.display()
    );

    // The case this command mostly exists for: profiles whose directories survived while
    // storage.json lost their entries.
    let state = State::load(layout)?;
    let live: BTreeSet<&str> = state.profiles.iter().map(|p| p.location.as_str()).collect();
    let returning: Vec<String> = restore::restored_profile_dirs(&entries)
        .into_iter()
        .filter(|loc| !live.contains(loc.as_str()))
        .collect();
    if !returning.is_empty() {
        println!(
            "  {} restores {} profile director(ies) not currently registered: {}",
            "*".cyan(),
            returning.len(),
            returning.join(", ")
        );
    }

    if o.dry_run {
        println!();
        for e in &entries {
            println!("  {}", e.relative.display());
        }
        println!("\n{} nothing written", "dry run:".bold());
        return Ok(ExitCode::SUCCESS);
    }

    // VS Code rewrites storage.json on exit, so a restore performed underneath a running editor
    // is undone the moment it quits.
    if guard::is_running(layout.flavor) && !o.force_running {
        bail!(guard::running_message(layout.flavor));
    }

    println!(
        "\n{} files in the archive are overwritten. Files created since the backup are left in \
         place, so a profile made after it keeps its directory but disappears from the restored \
         storage.json.",
        "note:".yellow()
    );

    if !o.yes {
        if !io::stdin().is_terminal() {
            bail!("refusing to restore without confirmation on a non-interactive stdin; pass --yes");
        }
        print!("\nrestore {} file(s)? [y/N] ", entries.len());
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("aborted.");
            return Ok(ExitCode::from(2));
        }
    }

    // Restoring is destructive in its own right, so the state being replaced is snapshotted too.
    if !o.no_backup {
        let pre = backup::snapshot(&layout.user_dir, &snapshot_dir)?;
        println!("\npre-restore backup: {}", pre.display().dimmed());
    }

    let written = restore::extract(&archive, &layout.user_dir)?;
    println!("{} {} file(s)", "restored:".green().bold(), written.len());
    println!(
        "\n{} extension binaries are not in the archive. Run `vsorg inventory` to see what is \
         missing, then `vsorg apply` or let VS Code refetch.",
        "next:".bold()
    );

    Ok(ExitCode::SUCCESS)
}

fn cmd_bind(layout: &Layout, path: &Path, profile: &str) -> Result<ExitCode> {
    let state = State::load(layout)?;

    if !profile.eq_ignore_ascii_case(DEFAULT_PROFILE_NAME) && state.find(profile).is_none() {
        let known: Vec<&str> = std::iter::once(DEFAULT_PROFILE_NAME)
            .chain(state.profiles.iter().map(|p| p.name.as_str()))
            .collect();
        bail!("no profile named `{profile}` (known: {})", known.join(", "));
    }

    let abs = std::fs::canonicalize(path)
        .with_context(|| format!("resolving {}", path.display()))?;
    if !abs.is_dir() {
        bail!("{} is not a directory", abs.display());
    }

    println!(
        "{} VS Code owns the binding; run this and it persists for the folder:",
        "note:".yellow()
    );
    println!();
    println!("  {}", apply::bind_command(layout.flavor, &abs.to_string_lossy(), profile).bold());
    println!();

    // Report the existing binding so the user can see whether this is a change.
    let uri = format!("file://{}", url_encode_path(&abs.to_string_lossy()));
    if let Some(loc) = state.workspaces.get(&uri) {
        let current = if loc == DEFAULT_PROFILE_SENTINEL {
            DEFAULT_PROFILE_NAME.to_string()
        } else {
            state
                .profiles
                .iter()
                .find(|p| &p.location == loc)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| loc.clone())
        };
        println!("currently bound to: {}", current.bold());
    } else {
        println!("currently unbound (opens in the new-window default).");
    }

    Ok(ExitCode::SUCCESS)
}

/// VS Code stores workspace URIs percent-encoded. Encodes the characters it actually escapes in
/// paths; everything else is left as-is.
fn url_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' | '!' | '$' | '&'
            | '\'' | '(' | ')' | '*' | '+' | ',' | ';' | '=' | ':' | '@' => out.push(c),
            _ => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

fn cmd_doctor(layout: &Layout, manifest_path: Option<&Path>) -> Result<ExitCode> {
    let (state, inv) = load(layout)?;
    let mut problems = 0usize;

    println!("{}", "store".bold());

    let dangling = inv.dangling();
    if dangling.is_empty() {
        println!("  {} every referenced extension is present on disk", "ok".green());
    } else {
        problems += 1;
        println!(
            "  {} {} referenced but missing from disk — these will fail to activate: {}",
            "!!".red(),
            dangling.len(),
            join_ids(&dangling)
        );
    }

    let orphans = inv.orphans();
    if orphans.is_empty() {
        println!("  {} no orphaned extension folders", "ok".green());
    } else {
        println!(
            "  {} {} on disk but in no profile (leftovers from deleted profiles): {}",
            "!".yellow(),
            orphans.len(),
            join_ids(&orphans)
        );
    }

    // Packs expand at install time. Note this is informational: an extension declaring a pack may
    // still be a genuine extension in its own right (ms-python.python ships the Python support
    // *and* packs Pylance), so the fix is to declare the members too, never to drop the pack.
    let installed_packs: Vec<(&String, &Vec<String>)> = inv
        .packs
        .iter()
        .filter(|(id, _)| inv.referenced().contains(*id))
        .collect();
    if installed_packs.is_empty() {
        println!("  {} no extension packs installed", "ok".green());
    } else {
        println!(
            "  {} {} installed extension(s) pull in members at install time; declare the members \
             alongside them:",
            "i".blue(),
            installed_packs.len()
        );
        for (id, members) in &installed_packs {
            println!("      {id} -> {}", members.join(", "));
        }
    }

    println!();
    println!("{}", "profiles".bold());

    if state.profiles.iter().filter(|p| !p.is_builtin()).count() + 1 > 6 {
        println!(
            "  {} more than 6 profiles; each switch reloads the window — 4-5 is the practical ceiling",
            "!".yellow()
        );
    }

    for pe in &inv.profiles {
        if pe.extensions.is_empty() && pe.location.as_deref().is_some_and(|l| !l.starts_with("builtin/")) {
            println!("  {} `{}` has no extensions", "!".yellow(), pe.profile);
        }
    }

    // These re-prompt for sign-in in every profile they are installed into.
    let auth_gated = ["anthropic.claude-code", "openai.chatgpt", "github.copilot", "github.copilot-chat", "ms-vscode-remote.remote-containers", "ms-vscode.remote-server"];
    let hits: Vec<&str> = auth_gated
        .iter()
        .copied()
        .filter(|id| inv.referenced().contains(*id))
        .collect();
    if !hits.is_empty() {
        println!(
            "  {} auth-gated, expect a re-sign-in per profile: {}",
            "!".yellow(),
            hits.join(", ")
        );
    }

    if let Some(path) = manifest_path {
        let m = Manifest::load(path)?;
        println!();
        println!("{}", "manifest".bold());

        let declared = m.all_ids();

        let unknown: BTreeSet<String> = declared.difference(&inv.on_disk).cloned().collect();
        if unknown.is_empty() {
            println!("  {} every declared extension is already on disk", "ok".green());
        } else {
            println!(
                "  {} {} declared but not installed anywhere yet (will be fetched from the \
                 marketplace; a typo fails here): {}",
                "!".yellow(),
                unknown.len(),
                join_ids(&unknown)
            );
        }

        let unclassified: BTreeSet<String> =
            inv.referenced().difference(&declared).cloned().collect();
        if unclassified.is_empty() {
            println!("  {} every installed extension is classified", "ok".green());
        } else {
            println!(
                "  {} {} installed but absent from the manifest — they survive in profiles \
                 without `prune` and are removed from those with it. Add to a profile to keep, \
                 or to `ignore` to silence: {}",
                "!".yellow(),
                unclassified.len(),
                join_ids(&unclassified)
            );
        }

        // An undeclared pack member still gets installed, so with `prune` on it is uninstalled
        // and reinstalled on every run — the manifest can never converge.
        let unpinned = inv.unpinned_pack_members(&declared);
        if unpinned.is_empty() {
            println!("  {} every declared pack has its members pinned", "ok".green());
        } else {
            problems += 1;
            println!(
                "  {} {} declared pack(s) have members the manifest omits; they will be installed \
                 anyway and fight `prune`:",
                "!!".red(),
                unpinned.len()
            );
            for (pack, missing) in &unpinned {
                println!("      {pack} -> add {}", missing.join(", "));
            }
        }

        for (name, spec) in &m.profiles {
            if spec.prune {
                continue;
            }
            if let Some(pe) = inv.get(name) {
                let desired = m.desired(name).unwrap_or_default();
                let extra = pe.ids().difference(&desired).count();
                if extra > 0 {
                    println!(
                        "  {} `{name}` has {extra} extension(s) the manifest does not declare, \
                         and prune is off — they will persist",
                        "!".yellow()
                    );
                }
            }
        }
    }

    println!();
    if problems == 0 {
        println!("{} no blocking problems.", "ok:".green().bold());
        Ok(ExitCode::SUCCESS)
    } else {
        println!("{} {problems} blocking problem(s).", "problems:".red().bold());
        Ok(ExitCode::from(1))
    }
}

/// Keeps diagnostic lines readable when a set is large.
fn join_ids(ids: &BTreeSet<String>) -> String {
    const MAX: usize = 8;
    let shown: Vec<&str> = ids.iter().take(MAX).map(|s| s.as_str()).collect();
    if ids.len() > MAX {
        format!("{}, +{} more", shown.join(", "), ids.len() - MAX)
    } else {
        shown.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_uris_percent_encode_the_way_vs_code_stores_them() {
        // Captured from a real profileAssociations key.
        assert_eq!(
            url_encode_path("/Users/x/Personal/Intern Report"),
            "/Users/x/Personal/Intern%20Report"
        );
        assert_eq!(
            url_encode_path("/Users/x/GoogleDrive-a@gmail.com/My Drive"),
            "/Users/x/GoogleDrive-a@gmail.com/My%20Drive"
        );
        assert_eq!(url_encode_path("/opt/homebrew/etc/nginx"), "/opt/homebrew/etc/nginx");
    }

    #[test]
    fn id_lists_are_truncated_once_they_get_long() {
        let few: BTreeSet<String> = ["a.a", "b.b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(join_ids(&few), "a.a, b.b");

        let many: BTreeSet<String> = (0..12).map(|i| format!("p{i:02}.x")).collect();
        assert!(join_ids(&many).ends_with("+4 more"));
    }

    #[test]
    fn cli_parses_the_documented_invocations() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}

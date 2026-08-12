//! LLM-assisted partitioning of an installed extension set into stack-shaped profiles.
//!
//! Drawing profile boundaries is the one part of this tool that is genuinely a judgement call:
//! `sweetpad.sweetpad` is iOS work and `orta.vscode-jest` is web work, but nothing in the ID says
//! so. The extensions' own `package.json` files do say so — display name, description, categories,
//! contributed languages and debuggers — so the classifier reasons over [`ExtensionFacts`] rather
//! than over IDs.
//!
//! The model is treated as an untrusted suggestion engine, never as an authority:
//!
//! * it never runs a command — it returns JSON, which [`ingest`] validates and converts;
//! * every returned ID is checked against what is actually installed, so a hallucinated ID is an
//!   error rather than a marketplace fetch of something that may not exist;
//! * every installed extension must be accounted for exactly once, so nothing is silently dropped
//!   into a set that `prune` would then delete.
//!
//! Transport is a plain subprocess pipe — prompt on stdin, JSON on stdout — so any CLI works, and
//! the two halves are separately usable:
//!
//! ```text
//! vsorg classify --print-prompt | claude -p | vsorg classify --from-json - -o my.toml
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::manifest::{normalize, Manifest, Meta, ProfileSpec};
use crate::store::{ExtensionFacts, Inventory};

/// Knobs that shape the request. Defaults mirror the tool's stated position: partition by stack,
/// and stop at five profiles.
#[derive(Debug, Clone)]
pub struct Options {
    /// Upper bound on profiles, excluding Default.
    pub max_profiles: usize,
    /// Force these profile names instead of letting the model propose a partition.
    pub seed_profiles: Vec<String>,
    /// Route unassigned extensions to `ignore` instead of failing.
    pub allow_unassigned: bool,
    /// Emit `prune = true` in the generated manifest.
    pub prune: bool,
    /// Content kinds each generated profile should inherit from Default.
    pub shared: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_profiles: 5,
            seed_profiles: Vec::new(),
            allow_unassigned: false,
            prune: true,
            // Muscle memory should survive a profile switch; extensions and settings are exactly
            // what you wanted separated.
            shared: vec!["keybindings".into(), "snippets".into()],
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------------------------

/// The exact JSON shape [`ingest`] parses. Kept as one literal so the prompt and the parser
/// cannot drift apart.
const SCHEMA: &str = r#"{
  "base": ["publisher.name", ...],
  "profiles": {
    "<name>": {
      "description": "<one short line>",
      "extensions": ["publisher.name", ...]
    }
  }
}"#;

/// Render the full prompt: instructions plus one fact line per installed extension.
pub fn build_prompt(inv: &Inventory, opts: &Options) -> String {
    let mut s = String::new();

    s.push_str(
        "You are partitioning a VS Code extension set into profiles.\n\n\
         A VS Code profile activates per window, and its extensions all load together. The useful \
         partition axis is therefore the STACK or TOOLCHAIN a folder belongs to (iOS, JVM, web, \
         systems, infra, data), NOT a language paradigm and NOT a marketplace category. There is \
         no profile for \"functional programming\"; there is one for the projects you actually \
         open.\n\n\
         Rules:\n\
         1. Assign EVERY extension listed below exactly once — either to `base` or to exactly one \
         profile. Do not invent, rename, split, or omit any id.\n\
         2. `base` is for extensions that genuinely serve every stack: version control, editor \
         hygiene, themes and icons, spell checking, AI assistants, diagram tools. If an extension \
         is only useful when a particular toolchain is present, it belongs in a profile.\n\
         3. Extensions that must load together belong together. `dependsOn` entries below are \
         hard requirements: an extension and its dependencies must share a profile (or be in \
         base). `packMembers` entries are pulled in automatically at install time, so keep a pack \
         and its members together too.\n\
         4. Prefer few, broad profiles. Every switch reloads the window, so an over-split set is \
         worse than none.\n\
         5. Profile names: short, lowercase, no spaces (e.g. ios, jvm, web, systems, infra).\n\n",
    );

    if opts.seed_profiles.is_empty() {
        let _ = writeln!(
            s,
            "Propose at most {} profiles, plus `base`. Use fewer if the set does not justify {}.\n",
            opts.max_profiles, opts.max_profiles
        );
    } else {
        let _ = writeln!(
            s,
            "Use EXACTLY these profile names, no others: {}.\n",
            opts.seed_profiles.join(", ")
        );
    }

    s.push_str("Respond with JSON only. No prose, no markdown fences, no commentary.\n\n");
    s.push_str(SCHEMA);
    s.push_str("\n\n");

    let _ = writeln!(s, "Extensions ({}):\n", inv.on_disk.len());

    for id in &inv.on_disk {
        s.push_str(&fact_line(id, inv.facts.get(id)));
        s.push('\n');
    }

    s
}

/// One extension as a compact single line. Descriptions are truncated because a handful of
/// extensions ship paragraph-length ones, and the signal is all in the first clause.
fn fact_line(id: &str, facts: Option<&ExtensionFacts>) -> String {
    let Some(f) = facts else {
        // No readable package.json — the ID is all we have, and saying so beats implying the
        // extension has no capabilities.
        return format!("- {id} | (no metadata available)");
    };

    let mut parts: Vec<String> = Vec::new();

    if let Some(n) = &f.display_name {
        parts.push(n.clone());
    }
    if let Some(d) = &f.description {
        parts.push(truncate(d, 160));
    }
    if !f.categories.is_empty() {
        parts.push(format!("categories: {}", f.categories.join(", ")));
    }
    if !f.languages.is_empty() {
        parts.push(format!("languages: {}", join_capped(&f.languages, 12)));
    }
    if !f.debuggers.is_empty() {
        parts.push(format!("debuggers: {}", join_capped(&f.debuggers, 6)));
    }
    if !f.keywords.is_empty() {
        parts.push(format!("keywords: {}", join_capped(&f.keywords, 8)));
    }
    if !f.depends.is_empty() {
        parts.push(format!("dependsOn: {}", f.depends.join(", ")));
    }
    if !f.pack.is_empty() {
        parts.push(format!("packMembers: {}", f.pack.join(", ")));
    }

    format!("- {id} | {}", parts.join(" | "))
}

fn truncate(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

fn join_capped(items: &[String], max: usize) -> String {
    if items.len() <= max {
        return items.join(", ");
    }
    format!("{}, +{} more", items[..max].join(", "), items.len() - max)
}

// ---------------------------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------------------------

/// LLM CLIs tried in order when none is configured. Each must accept a prompt on stdin and write
/// its answer to stdout. Claude is preferred, Codex is the fallback.
///
/// Codex needs its flags spelled out: `exec` refuses to start outside a trusted directory without
/// `--skip-git-repo-check`, and `--sandbox read-only` holds it to what this task actually is —
/// classification returns JSON and has no business touching the filesystem. Its transcript-style
/// output, which echoes the prompt before answering, is handled by [`extract_json`].
const CANDIDATES: [(&str, &str); 4] = [
    ("claude", "claude -p"),
    ("codex", "codex exec --skip-git-repo-check --sandbox read-only -"),
    ("llm", "llm"),
    ("ollama", "ollama run llama3.2"),
];

/// Resolve the command to pipe the prompt through: `$VSORG_LLM`, else the first candidate whose
/// binary is on PATH.
pub fn detect_command() -> Result<String> {
    if let Ok(cmd) = std::env::var("VSORG_LLM") {
        if !cmd.trim().is_empty() {
            return Ok(cmd);
        }
    }
    for (binary, cmd) in CANDIDATES {
        if which(binary) {
            return Ok(cmd.to_string());
        }
    }
    Err(anyhow!(
        "no LLM CLI found on PATH (looked for: {}).\n\
         Set one explicitly with --llm '<command>' or $VSORG_LLM, or drive the pipe yourself:\n  \
         vsorg classify --print-prompt | <your-llm> | vsorg classify --from-json - -o out.toml",
        CANDIDATES.map(|(b, _)| b).join(", ")
    ))
}

fn which(binary: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let p = dir.join(binary);
                p.is_file() || p.with_extension("exe").is_file()
            })
        })
        .unwrap_or(false)
}

/// Pipe `prompt` into `command` and return its stdout.
///
/// The command runs through the platform shell so users can pass a full invocation with flags —
/// `--llm 'claude -p --model sonnet'` — which is the whole point of making the transport a pipe.
pub fn run_llm(command: &str, prompt: &str, timeout: Duration) -> Result<String> {
    let (shell, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };

    let mut child = Command::new(shell)
        .arg(flag)
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning `{command}`"))?;

    let mut stdin = child.stdin.take().expect("stdin piped");
    let payload = prompt.to_string();
    // Write from a thread: a prompt larger than the pipe buffer deadlocks otherwise, since the
    // child cannot drain it while we are still blocked writing.
    let writer = std::thread::spawn(move || stdin.write_all(payload.as_bytes()));

    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    let result = match rx.recv_timeout(timeout) {
        Ok(r) => r,
        Err(_) => {
            // The child keeps the pipe open; leaving it is worse than reporting the timeout, but
            // we cannot kill it after `wait_with_output` consumed the handle.
            bail!(
                "`{command}` produced no output within {}s — raise --timeout, or run the pipe \
                 manually with --print-prompt / --from-json",
                timeout.as_secs()
            )
        }
    };
    let _ = writer.join();
    let _ = handle.join();

    let out = result.context("waiting for LLM command")?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr
            .lines()
            .chain(stdout.lines())
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("no output");
        bail!("`{command}` exited {}: {detail}", out.status.code().unwrap_or(-1));
    }

    if stdout.trim().is_empty() {
        bail!("`{command}` succeeded but produced no output");
    }

    Ok(stdout)
}

// ---------------------------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawProfile {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    extensions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawResponse {
    #[serde(default)]
    base: Vec<String>,
    #[serde(default)]
    profiles: BTreeMap<String, RawProfile>,
}

/// What the model got wrong, reported together so one round-trip surfaces every problem.
#[derive(Debug, Default)]
pub struct Report {
    /// Returned IDs matching nothing installed — hallucinations, or typos.
    pub unknown: BTreeSet<String>,
    /// Installed IDs the model placed in more than one bucket.
    pub duplicated: BTreeSet<String>,
    /// Installed IDs the model never mentioned.
    pub unassigned: BTreeSet<String>,
    /// Dependency or pack edges the partition split across profiles.
    pub split_groups: Vec<String>,
}

/// Parse a raw LLM response into a validated manifest.
pub fn ingest(response: &str, inv: &Inventory, opts: &Options) -> Result<(Manifest, Report)> {
    let json = extract_json(response)?;
    let raw: RawResponse = serde_json::from_str(&json)
        .with_context(|| format!("LLM response was not the expected JSON shape:\n{json}"))?;

    if raw.profiles.is_empty() {
        bail!("LLM returned no profiles");
    }

    let installed = &inv.on_disk;
    let mut report = Report::default();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();

    // `home` maps each installed ID to its bucket: None = base, Some(profile).
    let mut home: BTreeMap<String, Option<String>> = BTreeMap::new();

    let mut record = |id: &str, bucket: Option<&str>, report: &mut Report| {
        let id = normalize(id);
        if !installed.contains(&id) {
            report.unknown.insert(id);
            return;
        }
        *seen.entry(id.clone()).or_insert(0) += 1;
        home.entry(id).or_insert_with(|| bucket.map(str::to_string));
    };

    for id in &raw.base {
        record(id, None, &mut report);
    }
    for (name, p) in &raw.profiles {
        for id in &p.extensions {
            record(id, Some(name), &mut report);
        }
    }

    report.duplicated = seen.iter().filter(|(_, n)| **n > 1).map(|(id, _)| id.clone()).collect();
    report.unassigned = installed.difference(&home.keys().cloned().collect()).cloned().collect();

    if !report.unknown.is_empty() {
        bail!(
            "LLM returned {} extension id(s) that are not installed: {}\n\
             The response cannot be trusted as a partition of the real set; re-run, or edit and \
             feed it back with --from-json.",
            report.unknown.len(),
            preview(&report.unknown)
        );
    }
    if !report.duplicated.is_empty() {
        bail!(
            "LLM assigned {} extension(s) to more than one bucket: {}",
            report.duplicated.len(),
            preview(&report.duplicated)
        );
    }
    if !report.unassigned.is_empty() && !opts.allow_unassigned {
        bail!(
            "LLM left {} installed extension(s) unassigned: {}\n\
             They would be pruned away. Re-run, or pass --allow-unassigned to route them to \
             `ignore` (kept, but in no profile).",
            report.unassigned.len(),
            preview(&report.unassigned)
        );
    }

    report.split_groups = find_split_groups(inv, &home);

    Ok((to_manifest(&raw, &home, &report, opts), report))
}

/// Dependency and pack edges the partition broke apart.
///
/// An `extensionDependencies` edge crossing profiles means the dependent silently fails to
/// activate; a pack edge means the member gets installed anyway and then fights `prune`. Either
/// way it is worth naming, but it is the user's call, so this is a warning and not an error.
fn find_split_groups(inv: &Inventory, home: &BTreeMap<String, Option<String>>) -> Vec<String> {
    let mut out = Vec::new();

    for (id, facts) in &inv.facts {
        let Some(owner) = home.get(id) else { continue };
        // Base is visible everywhere, so an edge out of base can never be split.
        if owner.is_none() {
            continue;
        }

        for (kind, edges) in [("depends on", &facts.depends), ("packs", &facts.pack)] {
            for dep in edges {
                let Some(dep_owner) = home.get(dep) else { continue };
                if dep_owner.is_none() || dep_owner == owner {
                    continue;
                }
                out.push(format!(
                    "{id} ({}) {kind} {dep} ({})",
                    owner.as_deref().unwrap_or("base"),
                    dep_owner.as_deref().unwrap_or("base"),
                ));
            }
        }
    }

    out
}

fn to_manifest(
    raw: &RawResponse,
    home: &BTreeMap<String, Option<String>>,
    report: &Report,
    opts: &Options,
) -> Manifest {
    let base: Vec<String> = home
        .iter()
        .filter(|(_, bucket)| bucket.is_none())
        .map(|(id, _)| id.clone())
        .collect();

    let mut profiles: BTreeMap<String, ProfileSpec> = BTreeMap::new();

    for name in raw.profiles.keys() {
        let extensions: Vec<String> = home
            .iter()
            .filter(|(_, bucket)| bucket.as_deref() == Some(name.as_str()))
            .map(|(id, _)| id.clone())
            .collect();

        profiles.insert(
            name.to_ascii_lowercase(),
            ProfileSpec {
                description: raw.profiles[name].description.clone(),
                extensions,
                prune: opts.prune,
                shared: opts.shared.clone(),
                no_base: false,
            },
        );
    }

    // Default cannot be deleted, so it becomes the minimal profile: base only. The model is never
    // asked about it — that is a structural fact about VS Code, not a judgement call.
    profiles.entry("default".to_string()).or_insert_with(|| ProfileSpec {
        description: Some("Minimal — base only; Default cannot be deleted".into()),
        extensions: Vec::new(),
        prune: opts.prune,
        shared: Vec::new(),
        no_base: false,
    });

    Manifest {
        meta: Meta { version: 1 },
        base,
        ignore: report.unassigned.iter().cloned().collect(),
        profiles,
    }
}

/// Pull the answer object out of a response that may be wrapped in fences, prose, or a full
/// transcript.
///
/// Taking the outermost braces is not good enough. Agent CLIs like `codex exec` echo the prompt
/// before answering, and this prompt *contains a JSON schema example* — so the outermost span runs
/// from that example's opening brace to the real answer's closing one, and parses as nothing.
/// Instead: enumerate every balanced top-level object and take the last one that is actually a
/// well-formed response. Last, because the answer follows the echo.
fn extract_json(response: &str) -> Result<String> {
    let text = response.trim();

    for candidate in balanced_objects(text).into_iter().rev() {
        if looks_like_response(candidate) {
            return Ok(candidate.to_string());
        }
    }

    Err(anyhow!(
        "no usable JSON object found in the LLM response.\n\
         Expected an object with `base` and `profiles` keys; got:\n{}",
        truncate(text, 400)
    ))
}

/// Parses, and carries the keys we asked for. A bare `{}` or the schema example (whose values are
/// the literal `...`) both fail here.
fn looks_like_response(candidate: &str) -> bool {
    serde_json::from_str::<RawResponse>(candidate)
        .map(|r| !r.profiles.is_empty())
        .unwrap_or(false)
}

/// Every balanced `{...}` span at nesting depth zero, in source order.
///
/// Brace counting is string-aware: a `}` inside a string literal (an extension description, say)
/// must not close the object, and `\"` must not end the string.
fn balanced_objects(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }

        match b {
            b'"' if depth > 0 => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start.take() {
                        // Byte indices land on `{`/`}`, which are ASCII, so this is char-safe.
                        out.push(&text[s..=i]);
                    }
                }
            }
            _ => {}
        }
    }

    out
}

fn preview(ids: &BTreeSet<String>) -> String {
    const MAX: usize = 10;
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
    use crate::store::ExtensionFacts;

    fn inv(ids: &[&str]) -> Inventory {
        let mut i = Inventory {
            on_disk: ids.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        for id in ids {
            i.facts.insert(
                id.to_string(),
                ExtensionFacts { id: id.to_string(), ..Default::default() },
            );
        }
        i
    }

    const GOOD: &str = r#"{
      "base": ["eamodio.gitlens"],
      "profiles": {
        "ios": { "description": "Swift", "extensions": ["sweetpad.sweetpad"] },
        "web": { "description": "TS", "extensions": ["orta.vscode-jest"] }
      }
    }"#;

    fn three() -> Inventory {
        inv(&["eamodio.gitlens", "sweetpad.sweetpad", "orta.vscode-jest"])
    }

    #[test]
    fn builds_a_manifest_that_partitions_the_installed_set() {
        let (m, report) = ingest(GOOD, &three(), &Options::default()).unwrap();
        assert_eq!(m.base, vec!["eamodio.gitlens"]);
        assert_eq!(m.profiles["ios"].extensions, vec!["sweetpad.sweetpad"]);
        assert_eq!(m.profiles["web"].description.as_deref(), Some("TS"));
        assert!(report.unassigned.is_empty());

        // base is unioned in, so each profile ends up with its own plus the shared set.
        assert_eq!(m.desired("ios").unwrap().len(), 2);
        // Default is synthesised as base-only regardless of what the model said.
        assert!(m.profiles.contains_key("default"));
        assert!(m.profiles["default"].extensions.is_empty());
    }

    #[test]
    fn rejects_ids_that_are_not_installed() {
        // The failure that matters: a hallucinated id would otherwise be fetched from the
        // marketplace on apply.
        let r = ingest(GOOD, &inv(&["eamodio.gitlens", "sweetpad.sweetpad"]), &Options::default());
        let e = r.unwrap_err().to_string();
        assert!(e.contains("not installed"), "{e}");
        assert!(e.contains("orta.vscode-jest"), "{e}");
    }

    #[test]
    fn rejects_an_extension_placed_in_two_buckets() {
        let dup = r#"{"base":["a.a"],"profiles":{"x":{"extensions":["a.a"]}}}"#;
        let e = ingest(dup, &inv(&["a.a"]), &Options::default()).unwrap_err().to_string();
        assert!(e.contains("more than one bucket"), "{e}");
    }

    #[test]
    fn unassigned_extensions_fail_closed_but_can_be_routed_to_ignore() {
        let partial = r#"{"base":[],"profiles":{"x":{"extensions":["a.a"]}}}"#;
        let i = inv(&["a.a", "forgotten.ext"]);

        // Default: refuse, because prune would delete the forgotten one.
        let e = ingest(partial, &i, &Options::default()).unwrap_err().to_string();
        assert!(e.contains("unassigned"), "{e}");
        assert!(e.contains("forgotten.ext"), "{e}");

        let opts = Options { allow_unassigned: true, ..Default::default() };
        let (m, report) = ingest(partial, &i, &opts).unwrap();
        assert_eq!(m.ignore, vec!["forgotten.ext"]);
        assert_eq!(report.unassigned.len(), 1);
    }

    #[test]
    fn normalises_the_case_the_model_returns() {
        let mixed = r#"{"base":["EaModio.GitLens"],"profiles":{"x":{"extensions":[]}}}"#;
        let (m, _) = ingest(mixed, &inv(&["eamodio.gitlens"]), &Options::default()).unwrap();
        assert_eq!(m.base, vec!["eamodio.gitlens"]);
    }

    #[test]
    fn extracts_json_from_fenced_or_chatty_responses() {
        let fenced = "Here you go:\n```json\n{\"base\":[],\"profiles\":{\"x\":{}}}\n```\nDone!";
        assert!(extract_json(fenced).unwrap().starts_with('{'));

        let chatty = "Sure! {\"base\":[],\"profiles\":{\"x\":{}}} hope that helps";
        assert_eq!(extract_json(chatty).unwrap(), "{\"base\":[],\"profiles\":{\"x\":{}}}");

        assert!(extract_json("I cannot help with that.").is_err());
        assert!(extract_json("{}").is_err(), "an empty object is not a partition");
    }

    #[test]
    fn survives_a_transcript_that_echoes_the_prompt_first() {
        // Exactly what `codex exec` emits: banner, the echoed prompt (which contains our schema
        // example), then the answer, then a footer. Outermost-brace extraction would splice the
        // schema's `{` to the answer's `}` and parse as nothing.
        let transcript = format!(
            "OpenAI Codex v0.147.0\n--------\nworkdir: /x\nmodel: gpt\n--------\n\
             user\nRespond with JSON only.\n\n{SCHEMA}\n\nExtensions (2):\n\
             - a.a | thing\n\ncodex\n\
             {{\"base\":[\"a.a\"],\"profiles\":{{\"web\":{{\"extensions\":[\"b.b\"]}}}}}}\n\
             tokens used\n8,535\n"
        );

        let got = extract_json(&transcript).unwrap();
        let parsed: RawResponse = serde_json::from_str(&got).unwrap();
        assert_eq!(parsed.base, vec!["a.a"]);
        assert_eq!(parsed.profiles["web"].extensions, vec!["b.b"]);
    }

    #[test]
    fn picks_the_last_valid_object_when_the_answer_is_repeated() {
        // codex prints the final message twice — inline and again as the footer.
        let doubled = "codex\n{\"base\":[],\"profiles\":{\"a\":{}}}\ntokens used\n10\n\
                       {\"base\":[],\"profiles\":{\"b\":{}}}";
        let got = extract_json(doubled).unwrap();
        assert!(got.contains("\"b\""), "{got}");
    }

    #[test]
    fn braces_inside_strings_do_not_terminate_an_object() {
        // Extension descriptions really do contain braces, e.g. snippet syntax like ${1:name}.
        let tricky = r#"{"base":["a.a"],"profiles":{"x":{"description":"uses ${1:foo} and }","extensions":[]}}}"#;
        let got = extract_json(tricky).unwrap();
        let parsed: RawResponse = serde_json::from_str(&got).unwrap();
        assert_eq!(parsed.profiles["x"].description.as_deref(), Some("uses ${1:foo} and }"));
    }

    #[test]
    fn escaped_quotes_do_not_end_the_string_scan() {
        let tricky = r#"{"base":[],"profiles":{"x":{"description":"a \" then } brace","extensions":[]}}}"#;
        assert!(extract_json(tricky).is_ok());
    }

    #[test]
    fn claude_is_preferred_and_codex_is_the_fallback() {
        assert_eq!(CANDIDATES[0].0, "claude");
        assert_eq!(CANDIDATES[1].0, "codex");
        // codex exec will not start outside a trusted directory without this.
        assert!(CANDIDATES[1].1.contains("--skip-git-repo-check"));
        // Classification returns JSON; it has no business writing anything.
        assert!(CANDIDATES[1].1.contains("--sandbox read-only"));
    }

    #[test]
    fn warns_when_a_dependency_edge_crosses_profiles() {
        let mut i = inv(&["a.main", "a.dep", "base.thing"]);
        i.facts.get_mut("a.main").unwrap().depends = vec!["a.dep".into()];

        let split = r#"{"base":["base.thing"],"profiles":{
            "x":{"extensions":["a.main"]}, "y":{"extensions":["a.dep"]}}}"#;
        let (_, report) = ingest(split, &i, &Options::default()).unwrap();
        assert_eq!(report.split_groups.len(), 1);
        assert!(report.split_groups[0].contains("depends on"), "{:?}", report.split_groups);

        // Same profile: no warning.
        let together = r#"{"base":["base.thing"],"profiles":{
            "x":{"extensions":["a.main","a.dep"]}}}"#;
        let (_, report) = ingest(together, &i, &Options::default()).unwrap();
        assert!(report.split_groups.is_empty());
    }

    #[test]
    fn a_dependency_in_base_is_never_a_split() {
        // base is unioned into every profile, so the edge is always satisfied.
        let mut i = inv(&["a.main", "a.dep"]);
        i.facts.get_mut("a.main").unwrap().depends = vec!["a.dep".into()];
        let json = r#"{"base":["a.dep"],"profiles":{"x":{"extensions":["a.main"]}}}"#;
        let (_, report) = ingest(json, &i, &Options::default()).unwrap();
        assert!(report.split_groups.is_empty());
    }

    #[test]
    fn generated_manifest_passes_its_own_validation() {
        let (m, _) = ingest(GOOD, &three(), &Options::default()).unwrap();
        let round: Manifest = toml::from_str(&m.to_toml().unwrap()).unwrap();
        assert_eq!(round.desired("ios"), m.desired("ios"));
    }

    #[test]
    fn prompt_carries_the_metadata_the_model_needs_to_decide() {
        let mut i = inv(&["sweetpad.sweetpad"]);
        let f = i.facts.get_mut("sweetpad.sweetpad").unwrap();
        f.display_name = Some("SweetPad (iOS/Swift development)".into());
        f.description = Some("Develop Swift/iOS projects in VS Code".into());
        f.categories = vec!["Programming Languages".into()];
        f.languages = vec!["swift".into()];

        let p = build_prompt(&i, &Options::default());
        assert!(p.contains("SweetPad (iOS/Swift development)"));
        assert!(p.contains("languages: swift"));
        assert!(p.contains("categories: Programming Languages"));
        assert!(p.contains("at most 5 profiles"));
    }

    #[test]
    fn seeded_profiles_replace_the_propose_instruction() {
        let opts = Options {
            seed_profiles: vec!["ios".into(), "web".into()],
            ..Default::default()
        };
        let p = build_prompt(&inv(&["a.a"]), &opts);
        assert!(p.contains("EXACTLY these profile names, no others: ios, web"));
        assert!(!p.contains("Propose at most"));
    }

    #[test]
    fn extensions_without_a_package_json_are_still_listed() {
        // Dropping them would let the model return a partition missing an installed extension.
        let i = Inventory {
            on_disk: ["ghost.ext".to_string()].into_iter().collect(),
            ..Default::default()
        };
        let p = build_prompt(&i, &Options::default());
        assert!(p.contains("- ghost.ext | (no metadata available)"));
    }

    #[test]
    fn long_descriptions_are_truncated_not_dropped() {
        let long = "word ".repeat(200);
        let out = truncate(&long, 160);
        assert!(out.chars().count() <= 161, "got {}", out.chars().count());
        assert!(out.ends_with('…'));
    }
}

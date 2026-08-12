# vscode-organizer

Declarative VS Code profile management. One TOML manifest is the source of truth; `vsorg` diffs it
against your live install and reconciles.

## Why

**VS Code profiles do not inherit.** "New Profile from Default" is a one-time copy, not a live
link. Any shared base set — git tooling, EditorConfig, theme, spell-checker — is duplicated into
every profile you create and drifts independently from then on. Hand-curating profiles in the UI
does not scale past two or three.

So don't curate them. Declare them, and generate them.

The partition axis that pays off is **stack / workspace type**, not language paradigm. Extensions
bind to toolchains, language servers and project layouts — there is no extension for "functional
programming", there is one for Haskell and one for Rust — and a profile activates per window. The
partition should mirror how you actually open folders.

Four or five profiles is the practical ceiling. Each switch reloads the window; an eight-way split
you traverse three times an hour is worse than no split at all.

## Install

```bash
cargo install --path .
# or
cargo build --release && cp target/release/vsorg ~/.local/bin/
```

Requires the `code` CLI on `PATH` (Command Palette → *Shell Command: Install 'code' command in
PATH*).

## Commands

| Command | Mutates? | |
|---|---|---|
| `vsorg inventory [--json] [--verbose]` | no | Profiles, extension counts, bound folders, store health |
| `vsorg export <dir>` | no | Extension lists + settings, keybindings, snippets, tasks, bindings |
| `vsorg init [file.toml]` | no | Reverse-engineer a manifest from the live install |
| `vsorg classify [-o file.toml]` | no | Propose a stack-shaped partition via an LLM CLI |
| `vsorg plan -m <file>` | no | Coloured diff; exits 1 on drift |
| `vsorg apply -m <file>` | **yes** | Reconcile. `--dry-run` prints the exact `code` commands |
| `vsorg backup [dir]` | no | Snapshot `User/` to a timestamped `.tar.gz` |
| `vsorg restore [archive]` | **yes** | Put a snapshot back; autodetects the newest. `--dry-run` |
| `vsorg bind <path> <profile>` | no | Prints the command that persists a folder binding |
| `vsorg doctor [-m <file>]` | no | Packs, orphans, dangling entries, auth-gated extensions |

Global: `--flavor stable|insiders|vscodium`, `--user-data-dir <DIR>`.

## Manifest

```toml
# `base` must come BEFORE [meta] — keys after a table header belong to that table.
base = [
  "eamodio.gitlens",
  "editorconfig.editorconfig",
]

ignore = ["zhfjyq.vscode-plugin-drawio"]   # installed, deliberately unassigned

[meta]
version = 1

[profiles.default]              # Default cannot be deleted; make it the minimal profile
description = "Minimal — notes, config files, no project"
extensions = []
prune = true

[profiles.ios]
description = "Swift / iOS via SweetPad"
extensions = ["sweetpad.sweetpad", "swiftlang.swift-vscode"]
prune = true
shared = ["keybindings", "snippets"]
```

- **`base`** is unioned into every profile. This is the whole point: declared once, materialised
  into all of them, re-materialised whenever it changes. Opt out per profile with `no_base = true`.
- **`prune`** removes extensions present in the profile but absent from the manifest. **Off by
  default**, so `apply` is additive and cannot silently destroy an unrecorded setup.
- **`shared`** mirrors VS Code's `useDefaultFlags`. The CLI cannot set these, so they are reported
  as manual UI steps rather than written behind the editor's back.
- **`ignore`** silences `doctor` for extensions you keep installed but assign to nothing.

A worked five-profile manifest for a real 59-extension install is in
[`examples/prashan.toml`](examples/prashan.toml). To generate one instead of writing it, see
[`vsorg classify`](#classify) below.

## Classify

Where to draw profile boundaries is the one genuinely judgement-shaped part of this. `sweetpad.sweetpad`
is iOS work and `orta.vscode-jest` is web work, but nothing in the ID says so — while the
extensions' own `package.json` files do. `vsorg classify` feeds that metadata (display name,
description, categories, contributed languages and debuggers, dependencies, pack membership) to an
LLM and turns the answer into a manifest.

```bash
vsorg classify -o my-profiles.toml           # autodetects an LLM CLI
vsorg classify --llm 'claude -p' -o my.toml  # or name one
vsorg classify --profiles ios,jvm,web -o my.toml   # fix the profile set yourself
```

Transport is a plain subprocess pipe — prompt on stdin, JSON on stdout — so any CLI works, and the
two halves are separately usable:

```bash
vsorg classify --print-prompt | claude -p | vsorg classify --from-json - -o my.toml
```

Autodetection tries `$VSORG_LLM`, then, in order:

| | |
|---|---|
| `claude -p` | preferred |
| `codex exec --skip-git-repo-check --sandbox read-only -` | fallback |
| `llm` | |
| `ollama run llama3.2` | |

Codex needs its flags spelled out: `exec` refuses to start outside a trusted directory without
`--skip-git-repo-check`, and `--sandbox read-only` holds it to what the task actually is —
classification returns JSON and has no business touching the filesystem.

Codex also prints a transcript rather than a bare answer, **echoing the whole prompt back before
replying** — and the prompt contains a JSON schema example. Taking the outermost braces would
therefore splice that example's opening brace to the real answer's closing one and parse as
nothing. `classify` instead enumerates every balanced, string-aware `{...}` span and takes the last
one that is a well-formed response. Any chatty or fenced CLI works for the same reason.

### The model is a suggestion engine, not an authority

It never runs a command. It returns JSON, and `classify` refuses to write a manifest unless:

- **every returned ID is actually installed** — a hallucinated ID would otherwise be fetched from
  the marketplace on the next `apply`;
- **no extension lands in two buckets**;
- **no installed extension is left out** — an omission would be silently pruned away. Override with
  `--allow-unassigned`, which routes them to `ignore` instead.

It also warns when the partition splits a `extensionDependencies` edge (the dependent then fails to
activate) or separates a pack from its members (which fights `prune` forever). Those are warnings,
not errors — the call is yours.

`Default` is never put to the model: it cannot be deleted, so it is always synthesised as the
base-only minimal profile. That is a structural fact about VS Code, not a judgement call.

Output is a starting point. Read it, then `vsorg doctor -m` and `vsorg plan -m` it before applying.

### Recommended sharing split

Share **keybindings and snippets**; isolate **extensions and settings**. Keybinding drift across
profiles is the main ongoing annoyance — muscle memory breaks on every switch — while extensions
and settings are exactly what you wanted separated.

## How it works

Reads go straight to the JSON on disk; **every write goes through the `code` CLI.** The on-disk
schema shifts between VS Code releases and the editor rewrites `storage.json` on exit, so
hand-rolled writes are both version-fragile and liable to be clobbered.

Three sets that are easy to conflate, and that `vsorg` keeps distinct:

| | |
|---|---|
| `~/.vscode/extensions/<pub>.<name>-<ver>/` | Physical binaries, **shared by every profile**. Deleting a profile frees metadata, not disk. |
| `~/.vscode/extensions/extensions.json` | **The Default profile's** extension list — *not* a global registry. |
| `User/profiles/<loc>/extensions.json` | A named profile's list. |

An extension used only by a named profile is on disk and in that profile's manifest while being
absent from Default's. Anything on disk that appears in no manifest is an orphan; anything
referenced but missing from disk is dangling and will fail to activate. `vsorg doctor` reports both.

### What the CLI cannot do

- **`useDefaultFlags`** (the shared-content toggles) — UI only. Reported as manual steps.
- **`profileAssociations`** (folder → profile bindings) — VS Code owns these. `vsorg bind` prints
  the `code --profile <name> <folder>` command that makes the editor persist one itself.
- **Profile deletion** — irreversible, and it takes settings, keybindings, snippets, tasks and UI
  state with it. Never automated. Profiles present in VS Code but absent from the manifest are
  reported and left alone.

### Safety

- `apply` snapshots `User/` to a timestamped `.tar.gz` first (excluding `History/`,
  `workspaceStorage/` and `logs/`, which are bulky and regenerable). `--no-backup` opts out.
- Profile creation refuses to run while VS Code is open, since the editor can revert it on quit.
  `--force-running` proceeds with extension changes only.
- Actions run **create → install → uninstall**. Binaries are shared, so pruning Default before the
  new profile has claimed an extension would force a needless re-download.
- A failed action aborts the run; `--keep-going` continues.

### Extension packs

Packs expand at install time, so a pack ID never round-trips through `--list-extensions`. Declare
the members **alongside** the pack, not instead of it — `ms-python.python` ships the Python support
*and* packs Pylance, so dropping it would remove the thing you wanted. `doctor` reports any
declared pack whose members the manifest omits; left unfixed, those members get installed anyway
and fight `prune` forever.

### Auth-gated extensions

Copilot, Claude Code, ChatGPT, remote/tunnel and some vendor SDKs need a **re-sign-in per profile**.
`doctor` lists the ones it recognises.

## Backup and restore

`apply` and `restore` snapshot `User/` automatically; `vsorg backup` does it on demand, before you
change something by hand.

```bash
vsorg backup                     # -> ./vsorg-backups/vscode-user-<unix>.tar.gz
vsorg restore --dry-run          # autodetects the newest snapshot, lists what comes back
vsorg restore                    # put it back
vsorg restore ./vsorg-backups/vscode-user-1786549577.tar.gz   # or name one
```

With no archive named, `restore` looks in `./vsorg-backups`, then `./`, and takes the newest —
so it works both from a project directory and from inside the backup directory itself. It prints
where it found the snapshot before doing anything. `--backup-dir <DIR>` overrides the search and is
authoritative: a wrong path is an error, never a silent fallback.

A snapshot holds `storage.json` (the profile registry **and** folder bindings), every profile
directory, settings, keybindings, snippets and tasks. It excludes `History/`, `workspaceStorage/`
and `logs/` — bulky, regenerable, irrelevant to profile identity.

It does **not** hold extension binaries. Those live in `~/.vscode/extensions`, are shared across
every profile, and VS Code garbage-collects any that no profile references. After a restore,
`vsorg inventory` shows what is missing and `vsorg apply` refetches it.

The typical rescue: `storage.json` loses its `userDataProfiles` entries while the profile
directories survive on disk, so the profiles vanish from VS Code. A restore brings them and their
folder bindings straight back — and `restore` names the directories it is about to re-register
before writing anything.

Restoring is destructive in its own right, so it snapshots the current state first (`--no-backup`
opts out), refuses to run while VS Code is open, and asks before writing (`--yes` skips).

**Files created since the backup are left in place.** A profile made after the snapshot keeps its
directory but disappears from the restored `storage.json`. Nothing is deleted, so restoring the
newer snapshot brings it back.

Archive paths are treated as hostile: an entry that would escape `User/` aborts the restore before
a single byte is written.

## Migration runbook

Deletion is not undoable. Do this in order.

1. **Inventory and export first.**
   ```bash
   vsorg inventory --verbose
   vsorg export ./backup-$(date +%F)
   ```
   Also run Command Palette → *Profiles: Export Profile* for each profile you intend to delete —
   UI state is not reachable from the CLI.

2. **Generate a manifest, then edit it into the partition you want.**
   ```bash
   vsorg classify -o my-profiles.toml   # LLM proposes the partition
   # or, to just mirror what you already have:
   vsorg init my-profiles.toml
   ```

3. **Check it, then review the diff.**
   ```bash
   vsorg doctor -m my-profiles.toml
   vsorg plan  -m my-profiles.toml
   ```

4. **Dry run, then apply.** Quit VS Code first if the plan creates profiles.
   ```bash
   vsorg apply -m my-profiles.toml --dry-run
   vsorg apply -m my-profiles.toml
   ```

5. **Apply the manual steps** `apply` printed (the `shared` toggles).

6. **Delete the superseded profiles** via Command Palette → *Profiles: Delete Profile*.

7. **Bind folders**, and set your most-used profile as *Use for New Windows*.
   ```bash
   vsorg bind ~/Development/my-api backend
   ```

8. **Keep it honest.** `vsorg plan` exits 1 on drift, so it works as a CI or pre-commit check
   against a manifest kept in your dotfiles.

## Development

```bash
cargo test                        # unit + integration over tests/fixtures/
cargo clippy --all-targets -- -D warnings
```

`tests/fixtures/` mirrors a real VS Code 1.133.0 install: mixed-case marketplace IDs, a built-in
profile sharing all content with Default, an extension present only in a named profile, an
extension pack, and an orphaned folder.

## License

MIT

# vsorg

Keep VS Code profiles reproducible with one TOML manifest.

`vsorg` reads your installed profiles, shows the difference from the manifest, and reconciles
extensions through VS Code's own `code` CLI. It does not write VS Code's internal profile JSON.

## Why use it

VS Code profiles do not inherit changes. A shared extension set copied into several profiles
eventually drifts. `vsorg` makes the shared set explicit with `base`, then materialises it into
each profile.

Use it to:

- inventory installed profiles, extensions, orphans, and missing binaries;
- generate a manifest from an existing installation;
- preview and safely apply profile changes;
- export or back up profile configuration;
- validate extension packs, dependency splits, and auth-gated extensions.

## Install

Install the latest release on macOS or Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/prashan-s/vsorg/main/install.sh | sh
```

The installer verifies the release checksum and installs `vsorg` to `~/.local/bin`. Override the
destination with `VSORG_INSTALL_DIR=/path/to/bin`. Ensure that directory is on `PATH`.

Each push to `main` publishes a versioned GitHub Release automatically.

To build from source, Rust 1.74+ is required:

```bash
cargo install --path .
```

For VS Code, install the command from the Command Palette: **Shell Command: Install `code`
command in PATH**.

## Quick start

Create a manifest from your current setup. It is safe: this only reads VS Code data.

```bash
vsorg init vscode-organizer.toml
```

Review the generated file, then check it against the live installation:

```bash
vsorg doctor -m vscode-organizer.toml
vsorg plan -m vscode-organizer.toml
```

`plan` exits with status `1` when it finds drift. Apply only after reviewing the plan:

```bash
vsorg apply -m vscode-organizer.toml --dry-run
vsorg apply -m vscode-organizer.toml
```

`apply` creates a backup before it changes anything. Pass `--yes` for non-interactive use.

## Manifest

```toml
base = [
  "eamodio.gitlens",
  "editorconfig.editorconfig",
]

[profiles.default]
description = "Minimal profile"
extensions = []
prune = true

[profiles.web]
description = "Web development"
extensions = ["esbenp.prettier-vscode", "dbaeumer.vscode-eslint"]
prune = true
shared = ["keybindings", "snippets"]
```

- `base` is added to every profile. Set `no_base = true` on a profile to opt out.
- `prune = true` removes extensions that are in that profile but absent from the manifest.
  Pruning is off by default.
- `shared` records VS Code's shared-content settings. VS Code exposes these only in its UI, so
  `vsorg` reports the required manual step instead of modifying internal state.

## Commands

| Command | Purpose |
| --- | --- |
| `vsorg inventory [--json]` | List profiles, extensions, bindings, and store health. |
| `vsorg init [file.toml]` | Generate a manifest from the current installation. |
| `vsorg plan -m <file>` | Show changes required to match a manifest. |
| `vsorg apply -m <file>` | Reconcile profiles and extensions. Backs up first. |
| `vsorg doctor [-m <file>]` | Find packs, orphans, dangling references, and auth-gated extensions. |
| `vsorg export <dir>` | Export extensions and profile content. |
| `vsorg backup [dir]` | Create a compressed backup of VS Code user data. |
| `vsorg restore [archive]` | Restore a backup; supports `--dry-run`. |
| `vsorg bind <path> <profile>` | Print the VS Code command that binds a folder to a profile. |
| `vsorg classify -o <file>` | Ask a local LLM CLI to propose a stack-based profile partition. |

Use `--flavor stable|insiders|vscodium` to select a VS Code build, or `--user-data-dir <dir>` to
operate on an alternate data directory.

## Safety

- Every mutation goes through the official `code` CLI.
- `apply` runs create, install, then uninstall, and stops at the first failure by default.
- Profile deletion is deliberately unsupported because it is irreversible.
- Extension binaries are shared by VS Code profiles; deleting a profile does not reclaim them.
- `restore` rejects archive paths that could escape the VS Code user directory.

## License

MIT. See [LICENSE](LICENSE).

# vsorg

Keep VS Code profiles reproducible with one TOML manifest.

`vsorg` keeps VS Code profiles in sync with a TOML manifest.

## Why use it

VS Code profiles do not inherit changes. A shared extension set copied into several profiles
eventually drifts. `vsorg` makes the shared set explicit with `base`, then materialises it into
each profile.

It can:

- inventory installed profiles, extensions, orphans, and missing binaries;
- generate a manifest from an existing installation;
- preview and safely apply profile changes;
- export or back up profile configuration;
- validate extension packs, dependency splits, and auth-gated extensions.

## Install

Install on Apple Silicon macOS, x86_64 Linux, or ARM64 Linux:

```bash
curl -fsSL https://github.com/prashan-s/vsorg/releases/latest/download/install.sh | sh
```

To build from source, Rust 1.74+ is required:

```bash
cargo install --path .
```

For VS Code, install the command from the Command Palette: **Shell Command: Install `code`
command in PATH**.

## Quick start

Create a manifest from your current setup:

```bash
vsorg init vscode-organizer.toml
```

Review it, then check for problems and planned changes:

```bash
vsorg doctor -m vscode-organizer.toml
vsorg plan -m vscode-organizer.toml
```

Apply after reviewing the plan:

```bash
vsorg apply -m vscode-organizer.toml --dry-run
vsorg apply -m vscode-organizer.toml
```

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

- `base` is added to every profile.
- `prune = true` removes undeclared profile extensions.
- `shared` keeps common content aligned; VS Code applies these settings in its UI.

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

Use `--flavor stable|insiders|vscodium` for another VS Code build.

## License

MIT. See [LICENSE](LICENSE).

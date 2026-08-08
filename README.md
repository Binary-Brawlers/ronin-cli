# Ronin CLI

[![CI](https://github.com/Binary-Brawlers/ronin-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/Binary-Brawlers/ronin-cli/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Binary-Brawlers/ronin-cli?display_name=tag)](https://github.com/Binary-Brawlers/ronin-cli/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Ronin is an open-source, terminal-native coding agent powered by multiple AI models through your Ronin account. The CLI runs its agent loop, repository tools, permission policy, session storage, and terminal interface locally. Authentication, model access, and usage settlement are provided by the hosted Ronin service.

## Features

- Multi-model coding through OpenRouter-backed Ronin accounts
- Resumable, branching local conversations
- Manual, accept-edits, plan, and auto permission modes
- Native file, search, patch, shell, web-search, and user-question tools
- Per-task round and credit budgets
- Checksum-verified self-updates on macOS, Linux, and Windows
- Local transcripts and OS-backed credential storage

## Install

macOS and Linux (Intel or ARM64):

```sh
curl -fsSL https://raw.githubusercontent.com/Binary-Brawlers/ronin-cli/main/install.sh | sh
```

Windows 10 or newer (64-bit), from PowerShell:

```powershell
irm https://raw.githubusercontent.com/Binary-Brawlers/ronin-cli/main/install.ps1 | iex
```

Both installers verify the downloaded archive against the release's SHA-256 manifest. Set `RONIN_INSTALL_DIR` to choose another installation directory, or `RONIN_VERSION=ronin-v0.2.2` to pin a release.

## Quick start

```sh
ronin login
cd your-project
ronin
```

Run `ronin doctor` to check credentials, API connectivity, balance, and model availability. Run `ronin --help` for commands and flags.

Configuration is loaded from `~/.ronin/config.toml`, then project `ronin.toml`, then `RONIN_*` environment variables. The default API is `https://chat-api.ronin.africa`; use `--api-url` or `RONIN_API_URL` for a self-hosted or development endpoint.

## Build from source

Ronin requires the Rust toolchain pinned in `rust-toolchain.toml`.

```sh
git clone https://github.com/Binary-Brawlers/ronin-cli.git
cd ronin-cli
cargo check --workspace
cargo test --workspace --locked
cargo install --path apps/cli --locked
```

The workspace contains the `ronin-cli` binary/runtime crate and the provider-independent `ronin-agent-core` agent loop.

## Contributing

Issues and pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) before making a change and report vulnerabilities according to [SECURITY.md](SECURITY.md).

The CLI and local agent runtime in this repository are licensed under the [MIT License](LICENSE). The hosted Ronin service and its server-side source are separate and are not covered by this repository's license.

## Releases

The public `ronin-cli` repository is the source of truth for CLI code. Changes made only in the private Ronin monorepo do not sync here automatically.

### 1. Merge the changes to release

Create a separate branch and pull request for each feature or fix:

```sh
git switch main
git pull --ff-only
git switch -c feat/descriptive-name

# Make the change, then validate it.
cargo check --workspace --all-targets
cargo fmt --all -- --check

git add .
git commit -m "feat(cli): describe the user-visible change"
git push -u origin feat/descriptive-name
gh pr create --title "feat(cli): describe the user-visible change"
```

Wait for CI and merge the pull request. Use descriptive pull-request titles because GitHub uses every merged PR between release tags to generate **What's Changed**, contributor credits, first-time contributors, and the full changelog.

### 2. Choose the version

- Patch, such as `0.2.3`: backward-compatible fixes and small improvements.
- Minor, such as `0.3.0`: backward-compatible features.
- Major, such as `1.0.0`: breaking changes or the first stable release.

Do not edit Cargo version files manually. The release script updates them in its version-only pull request.

### 3. Run the release

Install and authenticate [GitHub CLI](https://cli.github.com/) once, then start from a clean, current `main`:

```sh
gh auth status
git switch main
git pull --ff-only
git status --short

./scripts/release.sh 0.2.3
```

After confirmation, the script:

1. Validates the repository state and version number.
2. Updates `apps/cli/Cargo.toml` and `Cargo.lock`.
3. Runs Cargo checks and formatting.
4. Creates a version-only pull request and waits for CI.
5. Squash-merges the pull request and pushes `ronin-vX.Y.Z`.
6. Waits for macOS, Linux, and Windows artifacts plus `SHA256SUMS`.
7. Prints the published GitHub Release URL.

Use `./scripts/release.sh --yes 0.2.3` only for an intentional non-interactive release. If an interruption happens after the tag is pushed, run the same command again to resume release verification.

## Uninstall

On macOS or Linux:

```sh
rm ~/.local/bin/ronin
```

On Windows, remove `%LOCALAPPDATA%\Ronin\bin\ronin.exe` and remove its directory from your user `PATH`.

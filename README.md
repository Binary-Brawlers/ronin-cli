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

Maintainers merge feature and fix pull requests with descriptive titles, then run `./scripts/release.sh X.Y.Z` from a clean `main`. The script creates the version-only pull request, waits for CI, tags the merge, and waits for the checksum-protected GitHub Release artifacts. GitHub automatically builds **What's Changed**, contributor credits, first-time contributors, and the full changelog from the pull requests merged between tags.

## Uninstall

On macOS or Linux:

```sh
rm ~/.local/bin/ronin
```

On Windows, remove `%LOCALAPPDATA%\Ronin\bin\ronin.exe` and remove its directory from your user `PATH`.

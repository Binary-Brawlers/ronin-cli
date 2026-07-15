# Ronin CLI

Ronin is a terminal-native coding agent powered by multiple AI models through your Ronin account.

## Install

macOS and Linux (Intel or ARM64):

```sh
curl -fsSL https://raw.githubusercontent.com/Binary-Brawlers/ronin-cli/main/install.sh | sh
```

The installer downloads the correct archive for your system, verifies it against the published SHA-256 manifest, and installs `ronin` to `~/.local/bin`. Override the destination with `RONIN_INSTALL_DIR`.

## Sign in and start

```sh
ronin login
ronin
```

Ronin opens the WorkOS device sign-in flow and connects to `https://chat-api.ronin.africa`. Run `ronin doctor` to check credentials, API connectivity, balance, and model availability.

## Releases

Release archives and `SHA256SUMS` are available on the [Releases page](https://github.com/Binary-Brawlers/ronin-cli/releases). Set `RONIN_VERSION=ronin-v0.1.0` when running the installer to pin a specific release.

## Uninstall

```sh
rm ~/.local/bin/ronin
```

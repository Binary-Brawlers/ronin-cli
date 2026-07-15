#!/bin/sh
set -eu

REPOSITORY="${RONIN_REPOSITORY:-Binary-Brawlers/ronin-cli}"
VERSION="${RONIN_VERSION:-latest}"
INSTALL_DIR="${RONIN_INSTALL_DIR:-${HOME}/.local/bin}"

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux) os="unknown-linux-musl" ;;
  *) echo "ronin: unsupported operating system: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch="x86_64" ;;
  arm64|aarch64) arch="aarch64" ;;
  *) echo "ronin: unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

target="${arch}-${os}"
archive="ronin-${target}.tar.gz"
if [ "$VERSION" = "latest" ]; then
  base="https://github.com/${REPOSITORY}/releases/latest/download"
else
  base="https://github.com/${REPOSITORY}/releases/download/${VERSION}"
fi
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
curl -fsSL "${base}/${archive}" -o "${tmp}/${archive}"
curl -fsSL "${base}/SHA256SUMS" -o "${tmp}/SHA256SUMS"
grep "  ${archive}$" "${tmp}/SHA256SUMS" > "${tmp}/SHA256SUMS.selected"
(cd "$tmp" && if command -v sha256sum >/dev/null 2>&1; then sha256sum -c SHA256SUMS.selected; else shasum -a 256 -c SHA256SUMS.selected; fi)
tar -xzf "${tmp}/${archive}" -C "$tmp"
mkdir -p "$INSTALL_DIR"
install -m 0755 "${tmp}/ronin" "${INSTALL_DIR}/ronin"
echo "Installed ronin to ${INSTALL_DIR}/ronin"
case ":${PATH}:" in *":${INSTALL_DIR}:"*) ;; *) echo "Add ${INSTALL_DIR} to PATH to run ronin." ;; esac

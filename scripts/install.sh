#!/bin/sh
# kern one-line installer (Linux / macOS).
#
#   curl -fsSL https://raw.githubusercontent.com/yesitsfebreeze/kern/master/install.sh | sh
#
# Downloads the prebuilt `kern` binary for this platform from the latest GitHub
# release and installs it to ~/.local/bin (override with KERN_BIN_DIR).
set -eu

REPO="yesitsfebreeze/kern"
BIN_DIR="${KERN_BIN_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"

# Detect libc on Linux: prefer musl when the dynamic loader is musl.
libc="gnu"
if [ "$os" = "Linux" ]; then
  if [ -f /lib/ld-musl-x86_64.so.1 ] || [ -f /lib/ld-musl-aarch64.so.1 ] || \
     (command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl); then
    libc="musl"
  fi
fi

case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64)        target="x86_64-unknown-linux-$libc" ;;
      aarch64|arm64)       target="aarch64-unknown-linux-$libc" ;;
      armv7l|armv7)        target="armv7-unknown-linux-gnueabihf" ;;
      armv6l|arm)          target="arm-unknown-linux-gnueabihf" ;;
      riscv64)             target="riscv64gc-unknown-linux-gnu" ;;
      ppc64le|powerpc64le) target="powerpc64le-unknown-linux-gnu" ;;
      s390x)               target="s390x-unknown-linux-gnu" ;;
      *) echo "kern: no prebuilt binary for Linux/$arch (build from source)"; exit 1 ;;
    esac ;;
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64)        target="x86_64-apple-darwin" ;;
      *) echo "kern: no prebuilt binary for macOS/$arch"; exit 1 ;;
    esac ;;
  *)
    echo "kern: unsupported OS '$os' — use the Windows installer (install.ps1) or build from source"
    exit 1 ;;
esac

url="https://github.com/$REPO/releases/latest/download/kern-$target.tar.gz"
echo "kern: downloading $target ..."
mkdir -p "$BIN_DIR"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

if ! curl -fSL "$url" -o "$tmp/kern.tar.gz"; then
  echo "kern: download failed ($url)"
  echo "      no release yet? see https://github.com/$REPO/releases"
  exit 1
fi
tar xzf "$tmp/kern.tar.gz" -C "$tmp"
mv "$tmp/kern" "$BIN_DIR/kern"
chmod +x "$BIN_DIR/kern"

echo "kern: installed to $BIN_DIR/kern"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "kern: add $BIN_DIR to your PATH, e.g.  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac
echo "kern: next — register the MCP server:  claude mcp add kern -- $BIN_DIR/kern mcp"

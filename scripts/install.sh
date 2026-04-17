#!/usr/bin/env sh
set -eu

OWNER="posit-dev"
REPO="mcp-repl"
APP="mcp-repl"

require_supported_linux_runtime() {
  [ "$TARGET" = "x86_64-unknown-linux-gnu" ] || return 0

  GLIBC_VERSION="$(getconf GNU_LIBC_VERSION 2>/dev/null || true)"
  case "$GLIBC_VERSION" in
    glibc\ *) GLIBC_VERSION="${GLIBC_VERSION#glibc }" ;;
    *) return 0 ;;
  esac

  GLIBC_MAJOR="${GLIBC_VERSION%%.*}"
  GLIBC_MINOR="${GLIBC_VERSION#*.}"
  GLIBC_MINOR="${GLIBC_MINOR%%.*}"
  if [ "$GLIBC_MAJOR" -lt 2 ] || { [ "$GLIBC_MAJOR" -eq 2 ] && [ "$GLIBC_MINOR" -lt 35 ]; }; then
    echo "unsupported glibc version: ${GLIBC_VERSION}; need glibc 2.35+ (Ubuntu 22.04-compatible) for ${APP}-${TARGET}" >&2
    exit 1
  fi
}

verify_downloaded_binary() {
  if ! "$EXTRACTED_PATH" --help >/dev/null; then
    echo "downloaded ${APP}-${TARGET} is not compatible with this system" >&2
    exit 1
  fi
}

CHANNEL="stable"
if [ "${1:-}" = "--dev" ]; then
  CHANNEL="dev"
fi

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux) os="unknown-linux-gnu" ;;
  Darwin) os="apple-darwin" ;;
  *)
    echo "unsupported OS: $OS" >&2
    exit 1
    ;;
esac

case "$ARCH" in
  x86_64|amd64) arch="x86_64" ;;
  arm64|aarch64) arch="aarch64" ;;
  *)
    echo "unsupported arch: $ARCH" >&2
    exit 1
    ;;
esac

TARGET="${arch}-${os}"

case "$TARGET" in
  x86_64-unknown-linux-gnu|aarch64-apple-darwin) ;;
  *)
    echo "unsupported target: $TARGET" >&2
    exit 1
    ;;
esac

require_supported_linux_runtime

if [ "$CHANNEL" = "stable" ]; then
  URL="https://github.com/${OWNER}/${REPO}/releases/latest/download/${APP}-${TARGET}.tar.gz"
else
  URL="https://github.com/${OWNER}/${REPO}/releases/download/dev/${APP}-${TARGET}.tar.gz"
fi

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

ARCHIVE_PATH="${TMPDIR}/${APP}.tar.gz"
EXTRACTED_PATH="${TMPDIR}/${APP}-${TARGET}/${APP}"
INSTALL_DIR="${HOME}/.local/bin"

curl -fsSL "$URL" -o "$ARCHIVE_PATH"
tar -xzf "$ARCHIVE_PATH" -C "$TMPDIR"
verify_downloaded_binary

mkdir -p "$INSTALL_DIR"
install "$EXTRACTED_PATH" "${INSTALL_DIR}/${APP}"

echo "installed ${APP} to ${INSTALL_DIR}/${APP}"
echo "add ${INSTALL_DIR} to PATH if needed"

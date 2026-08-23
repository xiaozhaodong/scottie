#!/bin/bash
# Build Scottie.app locally and install it into /Applications.
#
# .github/scripts/bundle-macos.sh stays the single source of truth for the
# bundle layout, Info.plist and signing — this only drives it with the host's
# target triple and copies the result out of the DMG it leaves behind.
#
# Usage: scripts/brand-build.sh [-y]
#   -y                 replace an existing install without asking
#   SCOTTIE_TARGET     target triple      (default aarch64-apple-darwin)
#   SCOTTIE_ARCH       arch label         (default arm64)
#   SCOTTIE_DEST       install directory  (default /Applications)
set -euo pipefail

cd "$(dirname "$0")/.."

TARGET="${SCOTTIE_TARGET:-aarch64-apple-darwin}"
ARCH="${SCOTTIE_ARCH:-arm64}"
DEST="${SCOTTIE_DEST:-/Applications}"
APP_NAME="Scottie.app"

ASSUME_YES=0
[[ "${1:-}" == "-y" || "${1:-}" == "--yes" ]] && ASSUME_YES=1

# A local install never runs the in-app updater, so skip staging its helper and
# building the update zip — one less binary to compile and sign every time.
export TTY7_PACKAGE_UPDATE_ZIP="${TTY7_PACKAGE_UPDATE_ZIP:-0}"

VERSION="$(grep -m1 '^version = "' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"

# Replacing a bundle that is currently running leaves the live process pointing
# at deleted files — panes survive (the server owns them) but the GUI misbehaves
# until it is restarted, and the swap can fail halfway.
if pgrep -f "$DEST/$APP_NAME/Contents/MacOS/" >/dev/null 2>&1; then
    echo "Scottie is running — quit it first (⌘Q), then re-run." >&2
    exit 1
fi

echo "==> build $VERSION ($TARGET)"
BINS=(--bin tty7-app --bin tty7)
[[ "$TTY7_PACKAGE_UPDATE_ZIP" != "0" ]] && BINS+=(--bin tty7-updater)
cargo build --release --target "$TARGET" "${BINS[@]}"

echo "==> bundle"
bash .github/scripts/bundle-macos.sh "$TARGET" "$ARCH"

DMG="dist/scottie-${VERSION}-macos-${ARCH}.dmg"
if [[ ! -f "$DMG" ]]; then
    echo "expected $DMG, but the bundler did not produce it" >&2
    exit 1
fi

if [[ -e "$DEST/$APP_NAME" && "$ASSUME_YES" -eq 0 ]]; then
    read -rp "Replace $DEST/$APP_NAME? [y/N] " reply
    [[ "$reply" == [yY] ]] || { echo "aborted"; exit 1; }
fi

# The bundler moves the staged .app into the disk image rather than copying it
# (dist/ never holds two of them), so mount the DMG to get the bundle back.
MOUNT="$(mktemp -d)"
cleanup() {
    hdiutil detach "$MOUNT" -quiet 2>/dev/null || true
    rmdir "$MOUNT" 2>/dev/null || true
}
trap cleanup EXIT
hdiutil attach "$DMG" -mountpoint "$MOUNT" -nobrowse -quiet

echo "==> install into $DEST"
rm -rf "${DEST:?}/$APP_NAME"
# ditto, not cp -R: it preserves the adhoc signature and extended attributes.
ditto "$MOUNT/$APP_NAME" "$DEST/$APP_NAME"

echo "✅ $DEST/$APP_NAME  ($VERSION)"

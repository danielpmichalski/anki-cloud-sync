#!/usr/bin/env zsh
# Usage:
#   ./scripts/fork-anki-sync-server.zsh           # forks/upgrades to default tag (26.09.4)
#   ./scripts/fork-anki-sync-server.zsh 26.10     # forks/upgrades to a specific tag
#
# Copies rslib/, ftl/, proto/, .version and Cargo.lock from ankitects/anki at the
# given tag into the repo root. Safe to re-run for upgrades — Cargo.toml, README.md
# and our custom crates are left untouched.
#
# WARNING: rslib/ is deleted and replaced wholesale, which wipes our rslib delta
# (see README "rslib delta"). Before running, save it, e.g.:
#   diff -ruN <pristine-old-tag>/rslib rslib > rslib-delta.patch
# and re-apply afterwards with `patch -p1`. See docs/upgrades/ for past upgrade logs.
set -euo pipefail

ANKI_TAG=${1:-26.09.4}
REPO_ROOT=$(git rev-parse --show-toplevel)
TARGET_DIR="$REPO_ROOT"
TMP_DIR=$(mktemp -d)

cleanup() { rm -rf "$TMP_DIR"; }
trap cleanup EXIT

echo "==> Cloning ankitects/anki@$ANKI_TAG ..."
git clone --depth=1 --branch "$ANKI_TAG" \
  https://github.com/ankitects/anki.git "$TMP_DIR/anki"

echo "==> Initialising ftl submodules (translations required by anki_i18n build) ..."
git -C "$TMP_DIR/anki" submodule update --init --depth=1 ftl/core-repo ftl/qt-repo

echo "==> Replacing rslib/ ..."
rm -rf "$TARGET_DIR/rslib"
cp -r "$TMP_DIR/anki/rslib" "$TARGET_DIR/rslib"

echo "==> Replacing ftl/ ..."
rm -rf "$TARGET_DIR/ftl"
cp -r "$TMP_DIR/anki/ftl" "$TARGET_DIR/ftl"

echo "==> Replacing proto/ ..."
rm -rf "$TARGET_DIR/proto"
cp -r "$TMP_DIR/anki/proto" "$TARGET_DIR/proto"

echo "==> Copying .version ..."
cp "$TMP_DIR/anki/.version" "$TARGET_DIR/.version"

echo "==> Replacing Cargo.lock ..."
cp "$TMP_DIR/anki/Cargo.lock" "$TARGET_DIR/Cargo.lock"

echo ""
echo "Done. ankitects/anki@$ANKI_TAG rslib/ is now at rslib/"
echo ""
echo "Next steps (from the repo root):"
echo "  1. Re-apply our rslib delta:  patch -p1 < rslib-delta.patch   (see README 'rslib delta')"
echo "  2. Update root Cargo.toml workspace deps to match upstream's Cargo.toml"
echo "     and rust-toolchain.toml / Dockerfile rust image to upstream's channel"
echo "  3. cargo build --bin anki-sync-server"
echo "  4. cargo test -p sync-platform-api -p sync-storage-backends -p sync-storage-server"
echo "  5. cargo test -p anki --lib sync::collection::tests"
echo "  6. docker build -t anki-cloud-sync:local ."

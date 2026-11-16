#!/usr/bin/env bash
#
# Capture the Flutter client's screenshots (phone fleet list, phone agent
# terminal, desktop rail + workspace) for the README, driving the real app
# against a hermetic server seeded with the demo workspace in fixture.sh.
#
# Run from inside the client nix shell, which is where flutter lives:
#   nix develop .#client -c docs/tool/capture-client.sh [edit]
#
# Headless by default when xvfb-run is available (the app never steals the
# desktop); force a real window with CC_NO_XVFB=1.
set -euo pipefail

# SCRIPTDIR, not the invocation dir: this script is run from anywhere.
# shellcheck source-path=SCRIPTDIR
# shellcheck source=fixture.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/fixture.sh"

CLIENT_DIR="$CC_REPO_ROOT/client"
case "${1:-all}" in
  all) CC_SHOT_EDIT_ONLY=false ;;
  edit) CC_SHOT_EDIT_ONLY=true ;;
  *) echo "usage: capture-client.sh [edit]" >&2; exit 2 ;;
esac

for tool in flutter cargo git tmux python3 curl; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "capture-client: required tool '$tool' not on PATH (enter the client nix shell first)" >&2
    exit 1
  }
done

trap cc_fixture_cleanup EXIT
cc_fixture_env
cc_write_config
cc_seed_fixture

# --- restart the server over the patched tree --------------------------------
#
# Seeding stops the server so the PR metadata can be written into state.json;
# this run is the one the app talks to, and it loads that patched state.
echo "screenshots: starting the server on $CC_BASE_URL…"
cc_start_server

# --- build the cdylib so frb's loader opens a current library ----------------
#
# Same shim as client/tool/e2e.sh: frb's generated ioDirectory is
# rust/target/release/, which is a symlink to debug unless a real release build
# exists.
echo "screenshots: building the client cdylib…"
(cd "$CLIENT_DIR/rust" && cargo build -q)
mkdir -p "$CLIENT_DIR/rust/target"
if [ ! -e "$CLIENT_DIR/rust/target/release" ] || [ -L "$CLIENT_DIR/rust/target/release" ]; then
  ln -sfT debug "$CLIENT_DIR/rust/target/release" 2>/dev/null || true
fi

# --- drive the app and write the images -------------------------------------
# Outside the temp tree on purpose: when a capture stalls part-way, whatever it
# did manage to render survives for inspection instead of being swept up with the
# fixture. Only a full run's images are copied into docs/images.
SHOT_DIR="$CC_REPO_ROOT/target/screenshots"
rm -rf "$SHOT_DIR"
mkdir -p "$SHOT_DIR"

runner=()
if [ -z "${CC_NO_XVFB:-}" ] && command -v xvfb-run >/dev/null 2>&1; then
  runner=(xvfb-run -a)
fi

echo "screenshots: driving the Flutter app…"
(cd "$CLIENT_DIR" && "${runner[@]}" flutter test integration_test/screenshots_test.dart -d linux \
  --dart-define=CC_E2E_BASE_URL="$CC_BASE_URL" \
  --dart-define=CC_E2E_TOKEN="$CC_TOKEN" \
  --dart-define=CC_SHOT_EDIT_ONLY="$CC_SHOT_EDIT_ONLY" \
  --dart-define=CC_SHOT_DIR="$SHOT_DIR")

shopt -s nullglob
shots=("$SHOT_DIR"/*.png)
[ "${#shots[@]}" -gt 0 ] || {
  echo "capture-client: the app produced no images" >&2
  exit 1
}
for shot in "${shots[@]}"; do
  cp "$shot" "$CC_IMAGES_DIR/$(basename "$shot")"
  echo "screenshots: wrote docs/images/$(basename "$shot")"
done

echo "screenshots: done."

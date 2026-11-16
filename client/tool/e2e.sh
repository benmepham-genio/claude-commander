#!/usr/bin/env bash
#
# Full-stack e2e runner: boot a HERMETIC claude-commander-server over throwaway
# state, then drive the real Flutter app against it with the integration_test
# suite on the Linux desktop target.
#
# Hermetic by construction:
#   * XDG_CONFIG_HOME / XDG_DATA_HOME are redirected into a mktemp'd tree, so the
#     server's config.toml, state.json AND its default worktrees dir
#     ($XDG_DATA_HOME/claude-commander/worktrees) all live under $TMP. The real
#     ~/.config and ~/.local/share are never touched (the `directories` crate
#     honours XDG on Linux).
#   * The server binds loopback with a fixed pre-shared token and a fresh git
#     repo to branch sessions from.
# Everything is torn down on exit (server killed, $TMP removed).
#
# Run from the client dir inside the nix client shell, e.g.:
#   nix develop .#client   -c client/tool/e2e.sh     # local (has a display)
#   nix develop .#clientCi -c xvfb-run client/tool/e2e.sh   # CI (headless)
#
# Requires: flutter (linux desktop), cargo, git, tmux on PATH.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CLIENT_DIR="$REPO_ROOT/client"
PORT="${CC_E2E_PORT:-8787}"
TOKEN="cctest-token"
BASE_URL="http://127.0.0.1:${PORT}"

for tool in flutter cargo git tmux; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "e2e: required tool '$tool' not on PATH (enter the client nix shell first)" >&2
    exit 1
  }
done

TMP="$(mktemp -d)"
export XDG_CONFIG_HOME="$TMP/cfg"
export XDG_DATA_HOME="$TMP/data"
# Disable usage telemetry. The server is opt-out-by-default with a baked ingest
# credential, so without this a *local* e2e run would POST a session_start to the
# production OpenObserve endpoint (frontend "claude-commander-server"). CI is
# already covered by the workflow-level DO_NOT_TRACK, but a developer running
# this script directly is not — so set it here too.
export DO_NOT_TRACK=1
# Isolate tmux too: the server spawns tmux inheriting this env, and tmux puts its
# socket under $TMUX_TMPDIR. Pointing it at the temp tree means every session the
# server creates lives in a throwaway tmux server we can kill wholesale — nothing
# leaks into the developer's / CI runner's default tmux server, even on failure.
export TMUX_TMPDIR="$TMP/tmux"
# CRITICAL: when this script runs from inside a tmux session, $TMUX is set and
# tmux clients resolve their socket from it IN PREFERENCE to $TMUX_TMPDIR — so
# without this unset, the spawned server's sessions land on (and the cleanup's
# kill-server NUKES) the developer's real tmux server, killing every session
# they have open. Unset both so all tmux activity stays in the temp socket dir.
unset TMUX TMUX_PANE
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$TMUX_TMPDIR"
REPO="$TMP/repo"

SERVER_PID=""
cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  # Kill the isolated tmux server ($TMUX_TMPDIR still exported, $TMUX unset, so
  # this can only ever hit the throwaway server in the temp dir), then drop the
  # temp tree.
  tmux kill-server 2>/dev/null || true
  rm -rf "$TMP" 2>/dev/null || true
}
trap cleanup EXIT

# -- a fresh committed repo for sessions to branch from --
git init -q "$REPO"
git -C "$REPO" config user.email "e2e@test.local"
git -C "$REPO" config user.name "E2E"
# Repo-local, not `-c`: the server under test also runs git in this repo and its
# worktrees, and a developer's global commit.gpgsign (say via a locked 1Password
# op-ssh-sign) must reach neither that nor the fixture commit below.
git -C "$REPO" config commit.gpgsign false
git -C "$REPO" config tag.gpgsign false
echo "# e2e" > "$REPO/README.md"
git -C "$REPO" add README.md
git -C "$REPO" commit -q -m "Initial commit"

# Keep the creation journey independent of locally discovered agent programs.
# The first configured program is the server's default in create-options.
mkdir -p "$XDG_CONFIG_HOME/claude-commander"
cat > "$XDG_CONFIG_HOME/claude-commander/config.toml" <<'EOF'
[[programs]]
label = "E2E shell"
command = "bash"
EOF

# -- build + launch the server against the hermetic XDG tree --
echo "e2e: building server…"
cargo build -q -p claude-commander-server
echo "e2e: starting server on $BASE_URL (state under $TMP)…"
"$REPO_ROOT/target/debug/claude-commander-server" \
  --bind 127.0.0.1 --port "$PORT" --token "$TOKEN" &
SERVER_PID=$!

# -- wait for liveness --
for _ in $(seq 1 60); do
  if curl -fsS "$BASE_URL/health" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "e2e: server exited before becoming healthy" >&2
    exit 1
  fi
  sleep 0.5
done
curl -fsS "$BASE_URL/health" >/dev/null || { echo "e2e: server never became healthy" >&2; exit 1; }
echo "e2e: server healthy."

# -- register the repo as a project ---------------------------------------------
# The create-session page picks a project from a dropdown of projects registered
# on the server; it no longer accepts a typed repo path. A fresh hermetic server
# has none, so without this the app correctly shows "No projects registered on the
# server" and there is no form to drive. Registered here rather than through the UI
# on purpose: the add-project journey (+ → prompt → addProject → refresh) is
# already covered by client/test/projects_page_test.dart, and driving it here would
# make the session journey depend on dialog internals and on chrome-specific
# navigation that varies by theme.
echo "e2e: registering $REPO as a project…"
curl -fsS -X POST "$BASE_URL/api/projects" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d "{\"path\": \"$REPO\"}" >/dev/null ||
  { echo "e2e: failed to register the project" >&2; exit 1; }

# -- build the cdylib into rust/target/debug so flutter_rust_bridge's loader
#    finds a CURRENT library. frb's generated ioDirectory is rust/target/release/
#    (symlinked to debug); `cargo test` only refreshes target/debug/deps, not the
#    top-level .so the loader opens, and a fresh checkout has none at all. Build
#    it explicitly and ensure the release->debug symlink exists. --
echo "e2e: building client cdylib…"
(cd "$CLIENT_DIR/rust" && cargo build -q)
mkdir -p "$CLIENT_DIR/rust/target"
# Only create the frb release->debug shim if there's no real release build to
# shadow (a manual `cargo build --release` leaves a populated release/ dir).
if [ ! -e "$CLIENT_DIR/rust/target/release" ] || [ -L "$CLIENT_DIR/rust/target/release" ]; then
  ln -sfT debug "$CLIENT_DIR/rust/target/release" 2>/dev/null || true
fi

# -- drive the app end-to-end on the Linux desktop target --
#
# The e2e journey is named explicitly rather than running the whole
# integration_test/ directory: it also holds screenshots_test.dart, which renders
# the README images from a *different* fixture and is driven by
# docs/tool/capture-client.sh.
cd "$CLIENT_DIR"
flutter test integration_test/app_flows_test.dart -d linux \
  --dart-define=CC_E2E_BASE_URL="$BASE_URL" \
  --dart-define=CC_E2E_TOKEN="$TOKEN" \
  --dart-define=CC_E2E_REPO="$REPO"

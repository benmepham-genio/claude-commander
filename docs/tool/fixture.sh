#!/usr/bin/env bash
#
# Shared fixture for the README screenshots: builds a hermetic claude-commander
# world (config, state, worktrees, tmux server, three demo repos, ten demo
# sessions) that both capture scripts render from. Sourced, not executed.
#
# Hermetic by construction, same contract as client/tool/e2e.sh:
#   * XDG_CONFIG_HOME / XDG_DATA_HOME point into $CC_WORK, so config.toml,
#     state.json and the worktrees dir all live under the temp tree; the real
#     ~/.config and ~/.local/share are never read or written.
#   * TMUX_TMPDIR points into the temp tree AND $TMUX/$TMUX_PANE are unset, so
#     every tmux session lands on a throwaway server. Without the unset, tmux
#     resolves $TMUX in preference to $TMUX_TMPDIR and the cleanup's kill-server
#     would nuke the developer's real sessions.
#   * A `gh` stub that always fails shadows the real one: the PR poller treats a
#     gh failure as FetchFailed and preserves the seeded PR fields, where a real
#     gh would authoritatively report "no PR" and clear them.
#   * DO_NOT_TRACK=1, so a fixture run never posts telemetry.
set -euo pipefail

CC_TOOL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CC_REPO_ROOT="$(cd "$CC_TOOL_DIR/../.." && pwd)"
# shellcheck disable=SC2034  # read by the capture scripts that source this file
CC_IMAGES_DIR="$CC_REPO_ROOT/docs/images"

# --- hermetic environment ----------------------------------------------------

cc_fixture_env() {
  CC_WORK="$(mktemp -d)"
  export XDG_CONFIG_HOME="$CC_WORK/cfg"
  export XDG_DATA_HOME="$CC_WORK/data"
  export TMUX_TMPDIR="$CC_WORK/tmux"
  export DO_NOT_TRACK=1
  # CRITICAL: see the header — $TMUX beats $TMUX_TMPDIR for socket resolution.
  unset TMUX TMUX_PANE
  mkdir -p "$XDG_CONFIG_HOME/claude-commander" "$XDG_DATA_HOME" "$TMUX_TMPDIR" "$CC_WORK/bin"

  cat >"$CC_WORK/bin/gh" <<'STUB'
#!/usr/bin/env bash
# Fixture stub: fail so PR checks land on FetchFailed (cache-preserving).
echo "gh: unavailable in the screenshot fixture" >&2
exit 1
STUB
  chmod +x "$CC_WORK/bin/gh"

  # A `claude` on PATH that is the stand-in, so nothing in this tree can reach the
  # real agent — including the app's own default program. Each session gets its
  # own launcher too (see cc_agent_for); this is the backstop.
  ln -sf "$CC_TOOL_DIR/demo-claude.sh" "$CC_WORK/bin/claude"

  export PATH="$CC_WORK/bin:$PATH"

  CC_STATE="$XDG_DATA_HOME/claude-commander/state.json"
  CC_CONFIG="$XDG_CONFIG_HOME/claude-commander/config.toml"
  # Alongside the worktrees, because that is the one location the stand-in agent
  # can find from its own cwd. Created up front so the agent's wait is short.
  CC_MANIFEST="$XDG_DATA_HOME/claude-commander/worktrees/.demo-agents"
  mkdir -p "$(dirname "$CC_MANIFEST")"
  : >"$CC_MANIFEST"
}

cc_fixture_cleanup() {
  [ -n "${CC_SERVER_PID:-}" ] && kill "$CC_SERVER_PID" 2>/dev/null || true
  # $TMUX is unset and TMUX_TMPDIR is in the temp tree, so this can only reach
  # the throwaway server.
  tmux kill-server 2>/dev/null || true
  [ -n "${CC_WORK:-}" ] && rm -rf "$CC_WORK" 2>/dev/null || true
}

cc_write_config() {
  cat >"$CC_CONFIG" <<EOF
# Screenshot fixture config — hermetic, no network, no real agent.
tmux_tmpdir = "$TMUX_TMPDIR"
nix_develop = false
fetch_before_create = false
project_pull_enabled = false
pr_check_interval_secs = 86400
hibernate_enabled = false
ai_summary_enabled = false

[telemetry]
enabled = false

[[programs]]
label = "Claude"
command = "claude"

[[programs]]
label = "Codex"
command = "codex"

# The section list views are only meaningful with sections configured, and
# Section Stacks is the default view once they are — so the fixture declares the
# two sections that match the baked-in board defaults.
[[sections]]
name = "In Review"
pr_state = "open"

[[sections]]
name = "Merged"
pr_state = "merged"
EOF
}

# TUI-owned prefs (tui.json): pre-select a session so the preview pane has live
# agent output, and widen the list pane so titles and PR badges aren't clipped at
# the capture width. schema_version is stamped at the current value so the
# one-time view-mode migration doesn't clear what we set here.
#
# cc_write_tui_prefs <session title> [left pane pct]
cc_write_tui_prefs() {
  local title="$1" pct="${2:-42}"
  python3 - "$CC_STATE" "$XDG_DATA_HOME/claude-commander/tui.json" "$title" "$pct" <<'PY'
import json, sys

state_path, prefs_path, title, pct = sys.argv[1:5]
state = json.load(open(state_path))
session = next(s for s in state["sessions"].values() if s["title"] == title)
json.dump(
    {
        "schema_version": 1,
        "view_mode": "SectionStacks",
        "last_selected_session": session["id"],
        "last_selected_project": session["project_id"],
        "left_pane_pct": int(pct),
    },
    open(prefs_path, "w"),
    indent=2,
)
PY
}

# --- demo repositories -------------------------------------------------------

# A repo with enough history and files that diffs and the review view have
# something real to show.
cc_make_repo() {
  local name="$1" path="$CC_WORK/repos/$1"
  mkdir -p "$path"
  git init -q -b main "$path"
  git -C "$path" config user.email "demo@claude-commander.local"
  git -C "$path" config user.name "Claude Commander Demo"
  git -C "$path" config commit.gpgsign false
  mkdir -p "$path/src"
  cat >"$path/README.md" <<EOF
# $name

Demo repository for the Claude Commander screenshots.
EOF
  cat >"$path/src/pool.rs" <<'EOF'
//! Connection pooling.

pub struct Pool {
    max_size: usize,
    idle: Vec<Connection>,
}

impl Pool {
    pub fn new(max_size: usize) -> Self {
        Self { max_size, idle: Vec::new() }
    }

    pub fn acquire(&mut self) -> Option<Connection> {
        self.idle.pop()
    }

    pub fn release(&mut self, conn: Connection) {
        if self.idle.len() < self.max_size {
            self.idle.push(conn);
        }
    }
}
EOF
  cat >"$path/src/lib.rs" <<'EOF'
//! Demo crate.

pub mod pool;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
EOF
  git -C "$path" add -A
  git -C "$path" commit -q -m "Initial commit"
  echo "$path"
}

# --- the server, which is also the seeding API --------------------------------

# Sessions are created through the server's HTTP API rather than the CLI, for two
# reasons: `POST /sessions` takes a `stack_parent` (so a stacked session's branch
# genuinely forks from its parent's, which `claude-commander new` can't express),
# and the client capture then needs no TUI binary at all — that binary pulls
# core's default `audio` feature, which won't link in the client CI shell (no
# PipeWire). Only capture-tui.sh builds it.
CC_PORT="${CC_PORT:-8788}"
CC_TOKEN="cc-screenshots-token"
CC_BASE_URL="http://127.0.0.1:${CC_PORT}"

cc_start_server() {
  "$CC_REPO_ROOT/target/debug/claude-commander-server" \
    --bind 127.0.0.1 --port "$CC_PORT" --token "$CC_TOKEN" \
    >>"$CC_WORK/server.log" 2>&1 &
  CC_SERVER_PID=$!

  local _
  for _ in $(seq 1 60); do
    curl -fsS "$CC_BASE_URL/health" >/dev/null 2>&1 && return 0
    kill -0 "$CC_SERVER_PID" 2>/dev/null || break
    sleep 0.5
  done
  echo "fixture: server never became healthy" >&2
  cat "$CC_WORK/server.log" >&2
  return 1
}

# Stop the server and wait for it to exit, so its final state.json write has
# landed before anything reads or rewrites the file.
cc_stop_server() {
  [ -n "${CC_SERVER_PID:-}" ] || return 0
  kill "$CC_SERVER_PID" 2>/dev/null || true
  wait "$CC_SERVER_PID" 2>/dev/null || true
  CC_SERVER_PID=""
}

# cc_api <method> <path> [json body] — authenticated request, response on stdout.
cc_api() {
  local method="$1" path="$2" body="${3:-}"
  if [ -n "$body" ]; then
    curl -fsS -X "$method" "$CC_BASE_URL/api$path" \
      -H "Authorization: Bearer $CC_TOKEN" \
      -H 'Content-Type: application/json' \
      -d "$body"
  else
    curl -fsS -X "$method" "$CC_BASE_URL/api$path" \
      -H "Authorization: Bearer $CC_TOKEN"
  fi
}

# --- session seeding ---------------------------------------------------------

# Record a session's demo-agent row in the manifest the stand-in agent reads (see
# demo-claude.sh). Keyed by worktree directory name, which is what the agent can
# derive from its own cwd — so the program string itself stays a bare `claude`.
#
# cc_register_agent <session id> <mode> <task> [detail]
cc_register_agent() {
  local id="$1" mode="$2" task="$3" detail="${4:-}"
  local worktree
  worktree="$(cc_worktree_of "$id")"
  printf '%s\t%s\t%s\t%s\n' "$(basename "$worktree")" "$mode" "$task" "$detail" \
    >>"$CC_MANIFEST"
}

# cc_new_session <title> <repo path> <mode> <task> [detail] [stack parent id]
# Creates a real session (worktree + branch + tmux) running the demo agent, and
# echoes the new session id.
cc_new_session() {
  local title="$1" repo="$2" mode="$3" task="$4" detail="${5:-}" parent="${6:-}"
  local body id
  body="$(python3 - "$repo" "$title" "$parent" <<'PY'
import json, sys

repo, title, parent = sys.argv[1:4]
# A bare `claude` — resolved from PATH to the fixture's stand-in — so the app
# treats the session as a Claude harness and the client's session header reads
# like a real one. What the stand-in *does* comes from the manifest, not argv.
opts = {
    "project_path": repo,
    "title": title,
    "program": "claude",
    "base_branch": "main",
}
if parent:
    opts["stack_parent"] = parent
print(json.dumps(opts))
PY
)"
  id="$(cc_api POST /sessions "$body" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
  cc_register_agent "$id" "$mode" "$task" "$detail"
  echo "$id"
}

# The session's worktree path, as recorded in state.json.
cc_worktree_of() {
  python3 - "$CC_STATE" "$1" <<'PY'
import json, sys
state = json.load(open(sys.argv[1]))
print(state["sessions"][sys.argv[2]]["worktree_path"])
PY
}

# Give a session's worktree some changes so the diffstat and review view aren't
# empty. cc_dirty_worktree <session id> <added lines> [committed]
cc_dirty_worktree() {
  local id="$1" adds="$2" commit="${3:-yes}"
  local wt
  wt="$(cc_worktree_of "$id")"
  python3 - "$wt/src/pool.rs" "$adds" <<'PY'
import sys
path, adds = sys.argv[1], int(sys.argv[2])
src = open(path).read()
src = src.replace(
    "    pub fn acquire(&mut self) -> Option<Connection> {\n        self.idle.pop()",
    "    pub fn acquire(&mut self) -> Option<Connection> {\n"
    "        self.reap_expired();\n"
    "        self.idle.pop()",
)
src += "\nimpl Pool {\n" + "".join(
    f"    /// Reaper step {i}.\n    fn reap_{i}(&mut self) {{}}\n" for i in range(adds)
) + "}\n"
open(path, "w").write(src)
PY
  if [ "$commit" = "yes" ]; then
    git -C "$wt" add -A
    git -C "$wt" -c commit.gpgsign=false commit -q -m "Rework the pool"
  fi
}

# Rewrite state.json with the PR metadata, stack links and unread flags the
# screenshots need. Sections fall out of the PR state via the default section
# predicates, so nothing is pinned with section_override.
#
# cc_patch_state <<'JSON' … JSON   (a title -> patch map)
cc_patch_state() {
  local patch
  patch="$(cat)"
  python3 - "$CC_STATE" "$patch" <<'PY'
import json, sys
from datetime import datetime, timedelta, timezone

path, patch = sys.argv[1], json.loads(sys.argv[2])
state = json.load(open(path))
sessions = state["sessions"]
by_title = {s["title"]: s for s in sessions.values()}

# Spread creation/activity times so "oldest in section first" ordering and the
# relative-age column look like a real workspace rather than one big batch.
now = datetime.now(timezone.utc)
for offset, (title, fields) in enumerate(patch.items()):
    s = by_title[title]
    age = timedelta(hours=2 + offset * 5)
    s["created_at"] = (now - age).isoformat().replace("+00:00", "Z")
    s["last_active_at"] = (now - age / 4).isoformat().replace("+00:00", "Z")
    s["entered_section_at"] = s["last_active_at"]
    for key, value in fields.items():
        if key == "stack_parent":
            s["stack_parent_session_id"] = by_title[value]["id"]
            s["pr_base_branch"] = by_title[value]["branch"]
            continue
        s[key] = value
    if s.get("pr_number") and not s.get("pr_base_branch"):
        s["pr_base_branch"] = "main"
    if s.get("pr_state") == "merged":
        s["pr_merged"] = True

json.dump(state, open(path, "w"), indent=2)
PY
}

# --- the fixture itself ------------------------------------------------------

# The demo workspace both screenshots render: ten sessions over three projects,
# spanning the default sections (In Progress catch-all / In Review / Merged),
# with two PR stacks and a mix of agent states.
cc_seed_fixture() {
  echo "screenshots: building binaries…"
  # The TUI binary is only built when a caller asks for it (CC_NEED_TUI): it
  # pulls core's default `audio` feature, which needs PipeWire headers the client
  # CI shell doesn't carry.
  cargo build -q -p claude-commander-server ${CC_NEED_TUI:+-p claude-commander}
  # shellcheck disable=SC2034  # capture-tui.sh launches this
  CC_BIN="$CC_REPO_ROOT/target/debug/claude-commander"

  echo "screenshots: starting the seeding server…"
  cc_start_server

  echo "screenshots: creating demo repositories…"
  local api webapp infra
  api="$(cc_make_repo api-server)"
  webapp="$(cc_make_repo webapp)"
  infra="$(cc_make_repo infra)"

  echo "screenshots: creating demo sessions…"
  local pool rate terraform dark auth token staging kanban sqlx react
  pool=$(cc_new_session "Fix connection pool leak" "$api" working \
    "fix the connection leak on acquire() timeout" \
    "the timeout path returns early")
  # `rate` and `token` are stacked, so they are created with a stack parent: the
  # server forks their branch off the parent's rather than off main.
  rate=$(cc_new_session "Add rate limiting" "$api" waiting \
    "add a token-bucket limiter to the API" "" "$pool")
  terraform=$(cc_new_session "Terraform state migration" "$infra" working \
    "move the terraform state to the S3 backend")
  dark=$(cc_new_session "Dark mode toggle" "$webapp" idle \
    "add a dark mode toggle to settings")
  auth=$(cc_new_session "Refactor auth module" "$api" idle \
    "split auth into issuing and verification")
  token=$(cc_new_session "Add token refresh endpoint" "$api" idle \
    "add a refresh endpoint on the auth split" "" "$auth")
  staging=$(cc_new_session "Add staging cluster" "$infra" idle \
    "stand up a staging cluster like prod")
  kanban=$(cc_new_session "Kanban board redesign" "$webapp" idle \
    "redesign the board columns to spec")
  sqlx=$(cc_new_session "Migrate to sqlx" "$api" idle \
    "replace diesel with sqlx in the queries")
  react=$(cc_new_session "Upgrade React" "$webapp" idle \
    "upgrade React to 19, fix deprecations")

  echo "screenshots: staging worktree changes…"
  cc_dirty_worktree "$pool" 7 no
  cc_dirty_worktree "$rate" 12
  cc_dirty_worktree "$terraform" 4 no
  cc_dirty_worktree "$dark" 9
  cc_dirty_worktree "$auth" 24
  cc_dirty_worktree "$token" 6
  cc_dirty_worktree "$staging" 15
  cc_dirty_worktree "$kanban" 31
  cc_dirty_worktree "$sqlx" 42
  cc_dirty_worktree "$react" 18

  # The PR metadata is written straight into state.json, so the server has to be
  # down first: it owns the file while running and would overwrite the patch.
  echo "screenshots: patching PR metadata and stacks…"
  cc_stop_server
  cc_patch_state <<'JSON'
{
  "Fix connection pool leak": {},
  "Add rate limiting": { "stack_parent": "Fix connection pool leak" },
  "Terraform state migration": {},
  "Dark mode toggle": { "unread": true },
  "Refactor auth module": {
    "pr_number": 470,
    "pr_url": "https://github.com/acme/api-server/pull/470",
    "pr_state": "open",
    "pr_reviewers": ["dtaylor"],
    "review_decision": "changes_requested"
  },
  "Add token refresh endpoint": {
    "pr_number": 483,
    "pr_url": "https://github.com/acme/api-server/pull/483",
    "pr_state": "open",
    "pr_draft": true,
    "stack_parent": "Refactor auth module"
  },
  "Add staging cluster": {
    "pr_number": 180,
    "pr_url": "https://github.com/acme/infra/pull/180",
    "pr_state": "open",
    "pr_labels": ["ready-for-test"],
    "pr_reviewers": ["rkhan"],
    "review_decision": "approved"
  },
  "Kanban board redesign": {
    "pr_number": 554,
    "pr_url": "https://github.com/acme/webapp/pull/554",
    "pr_state": "open",
    "pr_labels": ["dev-review-required"],
    "unread": true
  },
  "Migrate to sqlx": {
    "pr_number": 193,
    "pr_url": "https://github.com/acme/api-server/pull/193",
    "pr_state": "merged"
  },
  "Upgrade React": {
    "pr_number": 61,
    "pr_url": "https://github.com/acme/webapp/pull/61",
    "pr_state": "merged"
  }
}
JSON
}

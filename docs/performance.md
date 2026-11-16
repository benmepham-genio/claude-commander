# Responsiveness and refresh behavior

The performance work concentrates on keeping input handling independent of I/O,
refreshing after changes, and limiting rendering work to visible content. One
deliberate exception builds more than it shows: the browser review page, below.

| Area | Behavior |
| --- | --- |
| TUI event loop | Draws when events invalidate the view. Idle ticks do not rebuild session lists or boards. Event draining has an 8 ms / 128-event budget. Animation, preview acquisition and age/config maintenance have separate deadlines. |
| TUI actions | Keep-alive, creation reconciliation, post-mutation snapshot refreshes, local project registration and directory scans complete in background tasks. Refresh revisions prevent older responses replacing newer views. |
| Preview | The protocol supports pane, shell, full diff and stats-only requests; callers omitting the selector retain the full payload. Live captures use a 100 ms deadline; Info stats use one second. Superseded tasks are cancelled and queued stale results are rejected. ANSI parsing, dimming and line metrics are cached; rendering borrows the cached paragraph. |
| Remote refresh | Authenticated `/api/changes?since=...` waits up to 20 seconds. Successful mutations, observable state changes and clone completion wake it. Native reads always fetch current server state; the poller only hashes its own poll payloads. Native refresh work is coalesced to at most four cycles per second. Older servers keep their configured polling fallback. |
| Browser | Explicit refreshes stay independent of long waits. Healthy connections reconcile every 30 seconds; unsupported servers use 1.5 seconds. Hidden tabs consume change notifications too; unsupported servers retain the 1.5-second cadence. Tabs refresh on visibility. Ended-terminal recovery temporarily restores the fast cadence. Unchanged cadence updates preserve the existing deadline. |
| Flutter lists/review | Session rows, collapsed file headers, and review diff rows are built lazily with stable keys. Project/workspace membership indexes are reused per snapshot. Redundant nested store listeners were removed. Terminal throughput updates only its meter. Raw diff parsing has an exact-key cache limited to two reviews and 8 MiB of source keys. |
| Browser rendering | Unchanged project subtrees retain their DOM nodes. Review rows are built eagerly, in 100-row chunks marked `content-visibility: auto`, so every row is in the DOM for the browser's find-in-page while offscreen chunks skip style, layout and paint. Each chunk's stand-in height (`contain-intrinsic-block-size`) is carried across a rebuild from the old chunk's measured height. Comment anchors are indexed per file. |
| State/comments | No-op state mutations retain the existing durable file and generation. Pending-comment indexing is cached and invalidated on saves and external events, with a 30-second reconciliation limit. Nonrecursive watches cover only state/comments/reviewed directories, handle atomic replacement, and fall back to configured polling on watcher failure or directory replacement. |
| Git/pane caches | Concurrent misses for the same key share computation. Diff watchers cover only root directories, linked-worktree metadata and shared refs. All diff caches retain their configured TTL (500 ms by default), including for nested worktree edits; build trees are never registered recursively. Watch registration runs off async workers and is capped at 32 recent roots per cache, with LRU eviction. Removing a session or project releases its watchers, and roots deleted by another process are pruned when a new watch is registered. |
| Agent inspection | One tmux listing gathers active pane titles for a batch; conclusive titles avoid captures. Inconclusive inspection has bounded concurrency. Unsupported harnesses remain conservative. |

Periodic PR refresh, hourly project pulls, hibernation policy evaluation,
WebSocket heartbeats and input-reader handoff checks retain their existing
purposes. Terminal output continues to stream in order. Clone tracking wakes on
change notifications and retains its compatibility timeout. Pane scrollback is
bounded at the existing 1,000 lines, preserving scrolling behavior.

## Regression evidence

The tests check idle-frame invalidation, stale preview tokens, removed-backend
completion, refresh ordering, no-op writes, atomic file replacement, tracked
lockfile changes, shared diff cache invalidation, stats-only counts, authenticated
mutation wakeups, legacy fallback, payload failure recovery, snapshot request
counts, change-wait cleanup, and lazy large-diff rendering.

Flutter's 5,000-row fixture checks that fewer than 100 intrinsic-height row
widgets are mounted. The browser's 5,000-row fixture (`web/e2e/review.spec.ts`)
checks bounded *rendering*, not bounded DOM construction: the last row is in the
DOM without scrolling, yet is not rendered until scrolled near, and
`window.find` reaches it. These are workload bounds, not wall-clock speed claims.

Eager construction is a trade-off, not a free win. To keep it cheap, a row is
two elements and a text node (the line numbers are the row's `::before`, from a data
attribute) and carries no listener of its own: one delegated click handler on
the review body looks the row's line up in a `WeakMap`.

Measured in the e2e Chromium (rebuild via Refresh over a synthetic one-file
diff; median of 7 rebuilds; time from clearing the old rows to the end of the
synchronous rebuild, and to the next frame; JS heap read straight after the
rebuild, then again after a forced GC):

| Lines | Build | Rebuild (sync / to next frame) | Heap after rebuild | Heap after GC |
| --- | --- | --- | --- | --- |
| 5,000 | lazy chunks (before) | ~2 ms / ~14 ms | ~6 MB | ~5 MB |
| 5,000 | eager, row listeners and gutter spans | ~36 ms / ~55 ms | ~19 MB | ~9 MB |
| 5,000 | eager, delegated click and `::before` gutter | ~26 ms / ~43 ms | ~20 MB | ~7 MB |
| 50,000 | lazy chunks (before) | ~19 ms / ~109 ms | ~24 MB | ~12 MB |
| 50,000 | eager, row listeners and gutter spans | ~320 ms / ~430 ms | ~150 MB | ~58 MB |
| 50,000 | eager, delegated click and `::before` gutter | ~270 ms / ~365 ms | ~105 MB | ~39 MB |

So the slimmer rows cut retained memory by about a third, but the rebuild is
still a long task at 50,000 lines: its cost is creating the nodes at all, and
about 110 ms of it is removing the previous rows. The time difference between
the two eager rows is within this machine's run-to-run spread. When eager
construction was introduced, scrolling the whole diff measured less layout than
the lazy build, since nothing mounts on the way (not re-measured since). These are point measurements on one machine, not regression bounds;
re-measure before relying on them.

Run the full pinned checks with:

```sh
CC_FORCE_NIX=1 scripts/verify.sh --all
```

When the local default Flutter e2e port is occupied, set `CC_E2E_PORT` to an
available loopback port. Nix installations requiring explicit experimental
features can also set `NIX_CONFIG='experimental-features = nix-command flakes'`.

## Profiling after deployment

Use `scripts/dev-run.sh` for the relevant frontend. Compare identical fixtures
with 10, 100 and 500 sessions; a large single-file diff; many small files; long
ANSI captures; and multiple remote servers. Record:

- Input-to-draw latency and idle redraw/model-rebuild counts for the TUI.
- Flutter UI/raster frame times in profile mode and mounted row counts while scrolling.
- Browser long tasks, DOM row counts and network requests with visible/hidden tabs.
- Snapshot GETs and bytes, pane/Git subprocesses, and mutation-to-visible-update delay.
- Snapshot/comment-scan time and cache reuse under multiple clients.

Scorer FFI batching, terminal byte batching, layout-height changes and harness
specific lifecycle hooks require evidence from those profiles. Current code
preserves scoring, wrapping, terminal byte ordering and unsupported-agent
behavior. No production CPU, battery or frame-time improvement is asserted by
these source and regression checks.

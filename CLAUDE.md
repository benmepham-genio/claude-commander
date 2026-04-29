# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

- `cargo build` — debug build
- `cargo build --release` — release build (LTO enabled, symbols stripped, single codegen unit)
- `cargo test` — all tests (integration tests require tmux installed)
- `cargo test <test_name>` — single test
- `cargo clippy` — lint
- `cargo run -- --debug` — run TUI with debug logging to `/tmp/claude-commander.log`

## Coding conventions

- Minimise duplication: extract shared logic into helpers or existing utility functions rather than repeating code across modules
- Use idiomatic Rust patterns: leverage the type system, enums, pattern matching, iterators, and the `?` operator; prefer `impl Into<T>` / `AsRef<T>` over concrete types in function signatures where it improves ergonomics
- Follow the existing error handling style: `thiserror` derive macros for error enums, `Result<T>` alias from `error.rs`
- Use `tracing` macros (`info!`, `debug!`, `warn!`) for logging, not `println!` or `eprintln!` (except in CLI output paths in `main.rs`)

## Architecture

Terminal UI for managing Claude coding sessions via tmux and git worktrees. Each session gets an isolated git worktree, branch, and tmux session.

### Core flow

`App` (TUI main loop) owns a `SessionManager` which coordinates `TmuxExecutor` and `GitBackend`/`WorktreeManager`. State is shared via `Arc<RwLock<AppState>>` between the TUI and SessionManager.

### Modules

- **`session/`** — `types.rs` defines `Project` → `WorktreeSession` hierarchy (UUIDs, display as 8-char prefix). `manager.rs` orchestrates lifecycle: create/restart/delete sessions, content/diff retrieval.
- **`tmux/`** — `executor.rs`: async tmux commands with semaphore throttling (default 16 concurrent). `capture.rs`: cached pane content (50ms TTL, xxh3 hash-based staleness). `state.rs`: pattern-based agent state detection. `attach.rs`: PTY-based session attachment. `input.rs`: non-blocking input forwarding.
- **`git/`** — `backend.rs`: pure Rust git via gitoxide (gix crate). `worktree.rs`: uses git CLI for worktree mutations. `diff.rs`: cached diff computation (500ms TTL).
- **`tui/`** — `app.rs` (~1200 lines, largest file): main event loop, rendering, modal system, pane management. `event.rs`: `AppEvent`/`UserCommand` enums, key mappings, `EventLoop` multiplexing crossterm + ticks + state updates. `theme.rs`: auto-detects terminal color capability via COLORTERM/TERM. `widgets/`: TreeList, Preview, DiffView.
- **`config/`** — `settings.rs`: TOML config via figment, layered defaults → file → env vars (prefix `CC_`). `storage.rs`: JSON state persistence.
- **`error.rs`** — thiserror-based hierarchy: SessionError, TmuxError, GitError, ConfigError, TuiError.

### Key patterns

- Event-driven TUI: `EventLoop` combines terminal input, render ticks, and mpsc state update channels into a single `AppEvent` stream
- Caching with TTLs: ContentCapture (50ms) and DiffCache (500ms), both with hash-based change detection
- Modals: Input/Confirm/Help/Error overlay the main UI, handled in `app.rs`
- Background updater task periodically refreshes agent states for all active sessions

## Config and state files

Paths are determined by the `directories` crate (`ProjectDirs::from("com", "claude-commander", "claude-commander")`) and differ by platform:

| File | macOS | Linux |
|------|-------|-------|
| Config | `~/Library/Application Support/com.claude-commander.claude-commander/config.toml` | `~/.config/claude-commander/config.toml` |
| State | `~/Library/Application Support/com.claude-commander.claude-commander/state.json` | `~/.local/share/claude-commander/state.json` |
| Worktrees | `~/Library/Application Support/com.claude-commander.claude-commander/worktrees/` | `~/.local/share/claude-commander/worktrees/` |

See `Config` struct in `config/settings.rs` for all config fields. The worktrees directory can be overridden via the `worktrees_dir` config option.

## Testing

Unit tests are co-located in source files (`#[cfg(test)]`). Integration tests in `tests/integration_test.rs` require tmux. All async tests use `#[tokio::test]`.

### Test isolation

Tests must not read or modify anything on the real filesystem. Any disk access must go through `tempfile::TempDir` (already in dev-deps) for OS-portable temp paths. Never hardcode `/tmp/...` as a real path. Dummy `PathBuf` values stored in struct fields (never accessed on disk) are acceptable.

### Writing new tests

Use red-green TDD: write a failing test first, then implement the fix. Key areas covered by regression tests:

- **State management** (`config/storage.rs`) — bidirectional session-project linking, cascade delete, active session filtering
- **Status state machine** (`session/types.rs`) — transition guards, timestamp updates, display strings
- **Key mappings** (`tui/event.rs`) — every documented keybinding has a test; release/repeat events ignored
- **Config resolution** (`config/settings.rs`) — editor precedence chain, GUI editor auto-detection
- **Widget state** (`tui/widgets/`) — TreeListState navigation/wrap/clamp, PreviewState follow mode/scroll, DiffViewState
- **Caching** (`tmux/capture.rs`, `git/diff.rs`) — hash determinism, TTL staleness, parse_diff_stat edge cases
- **Name sanitization** (`session/manager.rs`) — branch name generation, special char handling
- **Error types** (`error.rs`) — all variant displays, type conversions

When adding new behavior, add a corresponding unit test that would fail without the change.

## Documentation

When adding or changing config options, hotkeys, or keybindings:

- **README.md** — Update the Keyboard Shortcuts table and the Configuration TOML block to reflect the change
- **Help modal** — Update the help text rendered in `app.rs` (`render_help_modal`) so the in-app `?` help stays in sync with the README
- **Settings modal** — Add new config options to `build_settings_rows()` in `app.rs` (General tab) and the corresponding `apply_settings_edit()` match arm so they are editable from the in-app settings UI
- **CLAUDE.md** — No update needed for individual options; the Architecture section points to `Config` struct as the source of truth

## Pre-commit hooks

This project uses [pre-commit](https://pre-commit.com/) to run `cargo fmt` and `cargo clippy` on every commit. After cloning, run:

```
pre-commit install
```

The `cargo fmt` hook auto-fixes formatting. If `cargo clippy` fails, fix the warnings before committing.

## Git conventions

**CRITICAL: Never force push under any circumstances. This includes `--force`, `--force-with-lease`, and amending commits that have been pushed. Always create new commits instead.**

- Branch names should be lowercase letters with hyphens separating words, no slashes e.g. `refactor-user-service`
- Pull request labels include `dev-review-required`, `ready-for-test`, `trivial`, `tidy`, `merge-on-ci-green`. Do not add labels unless instructed.
- Never skip GPG commit signing
- Precommit hooks may autoformat files while failing the commit; these changes will need to be restaged and the commit reattempted.
- Before committing, always ensure `cargo clippy` and `cargo build` pass with no warnings or errors. Fix any issues before creating the commit.
- Cutting a release: `cargo release {patch,minor,major} --execute` (see README). Never bump `Cargo.toml` manually.


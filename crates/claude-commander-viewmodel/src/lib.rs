//! Pure client-side decisions over a workspace snapshot.
//!
//! Every Claude Commander frontend — the ratatui TUI, the Flutter app, the
//! server's own callers — has to answer the same questions about a snapshot:
//! does this session match what the operator typed, and how do these rows rank?
//! Those answers are not wire contract and not host work, so they had nowhere to
//! live: they sat in [`claude_commander_core`], which is host-bound (gix,
//! pty-process, tmux, filesystem) and therefore unreachable from the Flutter
//! client's cdylib. The client re-implemented them in Dart instead, and the two
//! implementations drifted — `session_filter.dart` documented its own scorer as
//! "not byte-identical" to the Rust one.
//!
//! # What belongs here
//!
//! A decision a client makes *about* data it already holds. It renders nothing
//! and touches no I/O. If it needs a terminal, a file, a subprocess or a socket,
//! it belongs in `claude-commander-core` or in a frontend.
//!
//! Functions here take `&str`/scalars rather than DTOs wherever it is natural:
//! a core `WorktreeSession` and a protocol `SessionInfo` can then share one
//! implementation, and it crosses the Flutter client's FFI boundary without
//! marshalling a struct.
//!
//! # Why not `claude-commander-protocol`
//!
//! Both are shared and neither has host deps, so the split is about *who* has to
//! agree. Protocol is a contract with a peer over a wire: a running server and a
//! shipped client must agree on it, so changing it is a compatibility question.
//! This crate is shared only among in-tree frontends, which are rebuilt
//! together — so it can churn freely without any *wire* peer agreeing, while the
//! frontends themselves must still agree with each other, which is the whole
//! reason the logic is here rather than copied per client.
//!
//! Keeping them apart is what keeps "is this a wire change?" answerable.

pub mod query;
pub mod review;
pub mod workspace;

pub use query::{fuzzy_score, session_score};

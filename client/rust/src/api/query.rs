//! Session matching, bridged from `claude-commander-viewmodel`.
//!
//! The Dart side used to re-implement this scorer, and said so: *"The Rust core
//! matches with `SkimMatcherV2`; here we re-implement a case-insensitive
//! subsequence scorer in Dart (chosen over an FFI bridge). It won't be
//! byte-identical to Skim."* So the terminal and the app ranked the same session
//! list differently. These wrappers make both frontends call one implementation.
//!
//! # Why these are `#[frb(sync)]`
//!
//! Every other function in `crate::api` is async — frb runs the body on a worker
//! thread and Dart awaits a `Future`. That is right for anything doing I/O, and
//! wrong here: filtering runs on **every keystroke**, so an async scorer would
//! make the filter path async and let a slow frame deliver results for a query
//! the user has already moved past. `sync` keeps the call a plain function on the
//! Dart side, as the pure-Dart version was.
//!
//! Safe to run on Dart's UI isolate because the work is pure CPU over short
//! strings with no allocation beyond the score — the same work Dart was doing
//! inline before.
//!
//! Scalars in, scalar out: no struct crosses the boundary, so there is no
//! marshalling cost per row and the Dart signature stays trivial.
//!
//! # Why `i32` and not `fuzzy-matcher`'s `i64`
//!
//! frb maps `i64` to Dart's `PlatformInt64`, which is `int` natively but `BigInt`
//! on web. Nothing in this app's Dart handles one today and the protocol exposes
//! no `i64` at all, so returning one would make every comparison at every call
//! site carry a platform type — to express a relevance score of a few hundred.
//! Narrowing here keeps the Dart side a plain `int?`, which is also what the
//! pure-Dart scorer this replaces returned.

use flutter_rust_bridge::frb;

/// Score `needle` against `haystack`, Skim-style. `None` when `needle` is not a
/// subsequence of `haystack`; higher is a better match. An empty needle scores
/// `Some(0)` so a caller can filter without special-casing the empty query.
#[frb(sync)]
pub fn fuzzy_score(haystack: String, needle: String) -> Option<i32> {
    claude_commander_viewmodel::fuzzy_score(&haystack, &needle).map(narrow)
}

/// Best score across a session's title, branch and program — the ranking rule
/// the TUI's palette applies. The project name is deliberately not matched.
#[frb(sync)]
pub fn session_score(title: String, branch: String, program: String, query: String) -> Option<i32> {
    claude_commander_viewmodel::session_score(&title, &branch, &program, &query).map(narrow)
}

/// Narrow a score to `i32`, saturating.
///
/// Lossless, and provably so rather than just improbably: Skim computes scores
/// in `i32` (`fuzzy-matcher-0.3.7 src/skim.rs:350`, `MatrixCell.m_score`) and
/// widens to `i64` only on return, so no value reaching here can exceed the
/// range. Saturation is the belt: it is weakly monotone, so even an impossible
/// out-of-range score could only *tie* two entries, never invert the ordering
/// callers depend on.
fn narrow(score: i64) -> i32 {
    score.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

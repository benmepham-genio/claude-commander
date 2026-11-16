//! Kanban board model.
//!
//! Pure transformation from per-backend [`Snapshot`] DTOs + section
//! config into the board the TUI renders: a project sidebar (grouped per
//! server when more than one backend is configured) plus one column per
//! section ("In Progress" catch-all first, then the configured sections in
//! declared order). Each column holds *cards*, where a card is a single stack
//! unit — a PR stack rendered base-first with its children indented, or a lone
//! unstacked session as a one-row card. Sections span backends: cards from
//! every server land in the same shared columns.
//!
//! This lives in the library (not `tui/`) so the stack-grouping, section
//! resolution and flattening logic is unit-testable without a terminal, and it
//! consumes only wire DTOs so local and remote backends drive it identically.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use chrono::{DateTime, Utc};

use crate::api::{SessionInfo, Snapshot};
use crate::backend::{BackendId, ConnectionState};
use crate::session::{
    AgentState, IN_PROGRESS, ProjectId, SectionConfig, SessionId, SessionListItem, SessionNode,
    SessionStatus, resolve_stack_parent, stack_root, stack_top,
};

/// One backend's contribution to the board: its cached snapshot plus the
/// identity/health rendered on its sidebar heading.
pub struct BoardBackendInput<'a> {
    pub backend: BackendId,
    pub name: String,
    pub connection: ConnectionState,
    /// Set when this backend's server build is older than the client; rendered
    /// as a non-blocking `⚠` on its sidebar heading (independent of
    /// `connection`, so a mismatched-but-healthy server still shows its cards).
    pub version_warning: Option<crate::backend::VersionMismatch>,
    pub snapshot: &'a Snapshot,
    pub agent_states: &'a BTreeMap<SessionId, AgentState>,
}

/// A per-server grouping of sidebar entries. `projects` is the contiguous
/// range of this server's entries in [`Board::projects`]. The widget renders a
/// heading line above each group — suppressed when only one server exists, so
/// single-machine boards look exactly as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardServer {
    pub backend: BackendId,
    pub name: String,
    pub connection: ConnectionState,
    /// Set when this backend's server build is older than the client; the
    /// heading renders a `⚠` annotation (independent of `connection`).
    pub version_warning: Option<crate::backend::VersionMismatch>,
    pub projects: Range<usize>,
}

/// A project entry in the board's left sidebar.
///
/// Every project appears here, name-sorted within its server, including those
/// with no sessions (which contribute no cards to any column but still list
/// here with a count of zero). Pull-blocked state is intentionally *not*
/// carried: it lives in UI state (`UiState::project_pull_blocked`), not the
/// snapshot, so it is applied at render time like the LFS markers rather than
/// baked into the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardProjectEntry {
    pub project_id: ProjectId,
    pub name: String,
    pub session_count: usize,
}

/// One session card: a single [`SessionListItem::Worktree`] session rendered as
/// its own bordered box (session number + title in the border, status/markers/PR
/// and action buttons in the one interior line).
///
/// A PR stack is no longer one multi-row card: each member becomes its own card,
/// contiguous in the column, with stacked children flagged `indent = true` so the
/// widget draws them shifted right and narrower beneath their base.
///
/// `project_id` drives the card's border colour; `project_name` is retained so
/// palette/quick-switch consumers can map a session to its project straight from
/// the board (the name is never rendered on the card itself — project identity
/// lives in the border colour and the sidebar legend).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardCard {
    pub project_id: ProjectId,
    pub project_name: String,
    /// The session this card represents (always [`SessionListItem::Worktree`]).
    pub row: SessionListItem,
    /// True when this card is a stacked child of the card directly above it —
    /// the widget indents it one level. Mirrors the row's `stacked_child` flag.
    pub indent: bool,
}

/// One board column, corresponding to one section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardColumn {
    pub name: String,
    /// Advisory WIP limit resolved from config (`in_progress_limit` for the
    /// catch-all, `SectionConfig::max_sessions` otherwise). `None` = no limit.
    pub max_sessions: Option<u32>,
    pub cards: Vec<BoardCard>,
}

/// The full board: the (server-grouped) project sidebar plus the section
/// columns shared by every backend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Board {
    /// Per-server sidebar groupings, in backend order (local first).
    pub servers: Vec<BoardServer>,
    /// Sidebar entries, name-sorted within each server's range.
    pub projects: Vec<BoardProjectEntry>,
    /// Section columns. `columns[0]` is always the "In Progress" catch-all.
    pub columns: Vec<BoardColumn>,
}

/// A position on the board.
///
/// `col == 0` addresses the project sidebar; `col` in `1..=columns.len()`
/// addresses the section column at index `col - 1`. `row` is the selectable
/// row index within the target: the flattened Worktree-row index for a section
/// column (cards concatenated in order), or the index into
/// [`Board::projects`] for the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BoardPos {
    pub col: usize,
    pub row: usize,
}

impl Board {
    /// The single column-major traversal of the board's selectable worktree
    /// rows, yielding each row's [`BoardPos`] paired with its session id in
    /// session-number order. Every position/numbering accessor
    /// ([`position_of`](Self::position_of),
    /// [`pos_of_session_number`](Self::pos_of_session_number),
    /// [`session_numbers`](Self::session_numbers),
    /// [`worktree_count`](Self::worktree_count)) is defined in terms of this so
    /// the traversal exists in exactly one place. `row` is the flattened
    /// Worktree-row index within the column (cards concatenated in order).
    fn worktree_rows(&self) -> impl Iterator<Item = (BoardPos, SessionId)> + '_ {
        self.columns
            .iter()
            .enumerate()
            .flat_map(|(col_idx, column)| {
                column.cards.iter().enumerate().map(move |(row, card)| {
                    let SessionListItem::Worktree { id, .. } = &card.row else {
                        unreachable!("board rows are always Worktree")
                    };
                    (
                        BoardPos {
                            col: col_idx + 1,
                            row,
                        },
                        *id,
                    )
                })
            })
    }

    /// Every card across all columns, column-major then in-column order. Cards
    /// carry their project id/name, so palette/quick-switch consumers can build
    /// a session→project map straight from the board without a separate
    /// [`AppState`] lookup.
    pub fn cards(&self) -> impl Iterator<Item = &BoardCard> {
        self.columns.iter().flat_map(|column| column.cards.iter())
    }

    /// Total number of selectable worktree rows across all columns. One card is
    /// one session row now, so this is simply the card count.
    pub fn worktree_count(&self) -> usize {
        self.cards().count()
    }

    /// The `Worktree` row for `id`, if the session is on the board. Consumers
    /// that need session fields for rendering/lookups read them from here
    /// instead of a parallel flat list.
    pub fn find_worktree(&self, id: SessionId) -> Option<&SessionListItem> {
        self.cards().map(|card| &card.row).find(
            |item| matches!(item, SessionListItem::Worktree { id: row_id, .. } if *row_id == id),
        )
    }

    /// Column-major session numbering: session id → its 1-based number, matching
    /// the on-screen row number and
    /// [`pos_of_session_number`](Self::pos_of_session_number).
    pub fn session_numbers(&self) -> HashMap<SessionId, usize> {
        self.worktree_rows()
            .enumerate()
            .map(|(i, (_, id))| (id, i + 1))
            .collect()
    }

    /// Selectable-row counts per addressable column, consumed by the TUI's
    /// `widgets::board::BoardState::sync` (plain text, not an intra-doc link —
    /// that crate is downstream of core): `counts[0]` is the sidebar project
    /// count, `counts[1..]` each section column's flattened Worktree-row count.
    pub fn selectable_row_counts(&self) -> Vec<usize> {
        std::iter::once(self.projects.len())
            .chain(self.columns.iter().map(|column| column.cards.len()))
            .collect()
    }

    /// Locate a session on the board. Returns the 1-based column and the
    /// flattened Worktree-row index within that column, or `None` if the
    /// session isn't on the board.
    pub fn position_of(&self, id: SessionId) -> Option<BoardPos> {
        self.worktree_rows()
            .find(|(_, sid)| *sid == id)
            .map(|(pos, _)| pos)
    }

    /// The sidebar row index for `project_id` (column 0), or `None` if the
    /// project isn't listed. Shared by cursor re-anchoring and
    /// `select_project_in_sidebar` so both agree on where a project sits.
    pub fn sidebar_row_of(&self, project_id: ProjectId) -> Option<usize> {
        self.projects
            .iter()
            .position(|e| e.project_id == project_id)
    }

    /// The board position of the session with the given 1-based number (the Nth
    /// worktree row in column-major order), or `None` if out of range. Replaces
    /// the deleted free `session_number_to_list_index` + flat-list lookup.
    pub fn pos_of_session_number(&self, number: usize) -> Option<BoardPos> {
        number
            .checked_sub(1)
            .and_then(|i| self.worktree_rows().nth(i))
            .map(|(pos, _)| pos)
    }

    /// Resolve the session and/or project addressed by a board position.
    ///
    /// Sidebar rows (`col == 0`) yield `(None, Some(project))`; section-column
    /// rows yield `(Some(session), Some(project))`. Out-of-range positions —
    /// including landing on an empty column's header — yield `(None, None)`.
    pub fn ids_at(&self, pos: BoardPos) -> (Option<SessionId>, Option<ProjectId>) {
        if pos.col == 0 {
            return match self.projects.get(pos.row) {
                Some(entry) => (None, Some(entry.project_id)),
                None => (None, None),
            };
        }
        let Some(column) = self.columns.get(pos.col - 1) else {
            return (None, None);
        };
        match column.cards.get(pos.row) {
            Some(card) => {
                let SessionListItem::Worktree { id, project_id, .. } = &card.row else {
                    unreachable!("board rows are always Worktree")
                };
                (Some(*id), Some(*project_id))
            }
            None => (None, None),
        }
    }
}

/// Build the board model from every backend's cached snapshot and the
/// effective section config (callers pass
/// [`Config::effective_sections`](crate::config::Config::effective_sections)).
///
/// Columns are "In Progress" (the implicit catch-all) followed by `sections` in
/// declared order, shared across backends. Each PR stack becomes one card
/// placed in the section chosen by its newest leaf, walking `section_override`
/// closest-to-leaf-first (stale overrides skipped, falling back to the leaf's
/// `current_section`, then the catch-all). Within a column, cards are ordered
/// backend-major, then project-major (name-sorted), then by the leaf's
/// `entered_section_at` (leaf id as a stable tiebreaker).
///
/// When `filter` is `Some(project)`, only that project's cards are placed into
/// the columns; the sidebar (servers + projects) is unaffected, so every
/// project stays listed and reachable while the columns show one project's
/// work.
///
/// When `hide_empty` is true, a section with no cards (including the implicit
/// "In Progress" catch-all) is dropped from the columns entirely — so a board
/// with many configured sections, or a filter that empties most of them, shows
/// only the columns that have work.
pub fn build_board(
    inputs: &[BoardBackendInput<'_>],
    sections: &[SectionConfig],
    in_progress_limit: Option<u32>,
    filter: Option<ProjectId>,
    hide_empty: bool,
) -> Board {
    let valid_section = |name: &str| name == IN_PROGRESS || sections.iter().any(|s| s.name == name);

    let mut servers: Vec<BoardServer> = Vec::new();
    let mut projects: Vec<BoardProjectEntry> = Vec::new();
    // section name → its stack-group cards from every backend/project. Sorted
    // per column once all inputs are gathered (see the column assembly below):
    // by attention tier, then newest-first, so a column reads top-to-bottom in
    // rough order of how likely each session is to need the user.
    let mut pending_by_section: HashMap<String, Vec<PendingCard>> = HashMap::new();

    for input in inputs {
        let snapshot = input.snapshot;
        let by_id: HashMap<SessionId, &SessionInfo> = snapshot
            .sessions
            .iter()
            .map(|s| (s.session_id, s))
            .collect();

        // Stable project order. Ties on `name` fall back to id so ordering
        // never depends on snapshot ordering quirks.
        let mut sorted_projects: Vec<&crate::api::ProjectInfo> = snapshot.projects.iter().collect();
        sorted_projects.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));

        let group_start = projects.len();
        for project in &sorted_projects {
            projects.push(BoardProjectEntry {
                project_id: project.id,
                name: project.name.clone(),
                session_count: project.session_ids.len(),
            });
        }
        servers.push(BoardServer {
            backend: input.backend,
            name: input.name.clone(),
            connection: input.connection.clone(),
            version_warning: input.version_warning.clone(),
            projects: group_start..projects.len(),
        });

        for project in &sorted_projects {
            // An active project filter hides every other project's cards; the
            // sidebar entry above still lists the project so it stays reachable.
            if filter.is_some_and(|f| f != project.id) {
                continue;
            }
            // Sort by (created_at, id) so any downstream max_by_key (e.g.
            // fan-out children with identical created_at in `stack_top`) is
            // deterministic.
            let mut project_sessions: Vec<&SessionInfo> = project
                .session_ids
                .iter()
                .filter_map(|sid| by_id.get(sid).copied())
                .collect();
            if project_sessions.is_empty() {
                continue;
            }
            project_sessions.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then(a.session_id.cmp(&b.session_id))
            });

            // Bucket every session by its stack root (self for unstacked).
            // Track first-encounter root order so iteration is deterministic.
            let mut groups: HashMap<SessionId, Vec<&SessionInfo>> = HashMap::new();
            let mut group_roots: Vec<SessionId> = Vec::new();
            for s in &project_sessions {
                let root_id = stack_root(s.session_id, &project_sessions);
                if !groups.contains_key(&root_id) {
                    group_roots.push(root_id);
                }
                groups.entry(root_id).or_default().push(s);
            }

            let mut pending: Vec<PendingCard> = Vec::new();
            for root_id in group_roots {
                let members = groups.remove(&root_id).unwrap_or_default();
                // Pick the leaf in the whole subgraph; its section drives
                // placement.
                let leaf_id = stack_top(root_id, &project_sessions);
                let Some(leaf) = members.iter().find(|s| s.session_id == leaf_id).copied() else {
                    continue;
                };

                // Walk leaf → root. The first *valid* section_override wins;
                // stale overrides (naming a section no longer in config) are
                // skipped, and overrides on off-path siblings are never
                // considered.
                let mut effective: Option<String> = None;
                let mut cursor = leaf.session_id;
                for _ in 0..project_sessions.len() {
                    let Some(cur) = project_sessions
                        .iter()
                        .find(|s| s.session_id == cursor)
                        .copied()
                    else {
                        break;
                    };
                    if let Some(ovr) = &cur.section_override
                        && valid_section(ovr)
                    {
                        effective = Some(ovr.clone());
                        break;
                    }
                    match resolve_stack_parent(cur, &project_sessions) {
                        Some(parent) => cursor = parent,
                        None => break,
                    }
                }
                let section_name = effective
                    .or_else(|| leaf.current_section.clone())
                    .filter(|n| valid_section(n))
                    .unwrap_or_else(|| IN_PROGRESS.to_string());

                // Order within the group is stack-aware (root first, children
                // indented); build_session_order resolves parents only against
                // the slice given, so passing just the members keeps the root
                // flat and descendants indented even when the subgraph fans
                // out.
                let rows: Vec<SessionListItem> = build_session_order(&members)
                    .into_iter()
                    .filter_map(|(sid, stacked_child)| {
                        by_id
                            .get(&sid)
                            .map(|s| worktree_item(s, input.agent_states, None, stacked_child))
                    })
                    .collect();

                // Sort keys for the whole stack: its attention tier is the most
                // urgent member's (min), and its recency is the newest member's
                // `created_at`, so a stack floats by whichever of its sessions
                // most needs the user and, within a tier, newest-first.
                let tier = rows.iter().map(attention_tier).min().unwrap_or(1);
                let recency = rows
                    .iter()
                    .map(|item| {
                        let SessionListItem::Worktree { created_at, .. } = item else {
                            unreachable!("board rows are always Worktree")
                        };
                        *created_at
                    })
                    .max()
                    .unwrap_or_else(|| leaf.node_entered_section_at());

                pending.push(PendingCard {
                    section: section_name,
                    tier,
                    recency,
                    leaf_id,
                    project_id: project.id,
                    project_name: project.name.clone(),
                    rows,
                });
            }

            // Defer bucketing/sorting until every backend and project has
            // contributed, so a column orders by attention across all projects
            // rather than project-major.
            for pc in pending {
                pending_by_section
                    .entry(pc.section.clone())
                    .or_default()
                    .push(pc);
            }
        }
    }

    let columns: Vec<BoardColumn> = std::iter::once(IN_PROGRESS.to_string())
        .chain(sections.iter().map(|s| s.name.clone()))
        .filter_map(|name| {
            let mut groups = pending_by_section.remove(&name).unwrap_or_default();
            // With `hide_empty`, drop sections that produced no cards (including
            // the "In Progress" catch-all) so the board shows only columns with
            // work.
            if hide_empty && groups.is_empty() {
                return None;
            }
            let max_sessions = resolve_section_limit(&name, sections, in_progress_limit);
            // Order the column: attention tier ascending (needs-you at the top),
            // then newest-first within a tier, then leaf id as a stable
            // tiebreaker so equal-recency cards never jitter.
            groups.sort_by(|a, b| {
                a.tier
                    .cmp(&b.tier)
                    .then_with(|| b.recency.cmp(&a.recency))
                    .then_with(|| a.leaf_id.cmp(&b.leaf_id))
            });
            // Expand each stack group into its per-session cards, contiguous and
            // in stack order (base first, children indented).
            let cards: Vec<BoardCard> = groups
                .into_iter()
                .flat_map(|pc| {
                    pc.rows.into_iter().map(move |item| {
                        let SessionListItem::Worktree { stacked_child, .. } = &item else {
                            unreachable!("board rows are always Worktree")
                        };
                        BoardCard {
                            project_id: pc.project_id,
                            project_name: pc.project_name.clone(),
                            indent: *stacked_child,
                            row: item,
                        }
                    })
                })
                .collect();
            Some(BoardColumn {
                name,
                max_sessions,
                cards,
            })
        })
        .collect();

    Board {
        servers,
        projects,
        columns,
    }
}

/// A stack group awaiting placement: its resolved section plus the keys the
/// column sort needs (attention tier, recency, and a stable leaf-id tiebreak)
/// and the per-session rows to expand into cards.
struct PendingCard {
    section: String,
    tier: u8,
    recency: DateTime<Utc>,
    leaf_id: SessionId,
    project_id: ProjectId,
    project_name: String,
    rows: Vec<SessionListItem>,
}

/// Coarse attention tier for a session row: `0` = needs the user (waiting for
/// input, a paused cascade, or unread output), `1` = active (working, idle, or
/// a transient create/merge/push), `2` = stopped. Lower sorts nearer the top of
/// a column.
///
/// Deliberately coarse: a working↔idle flip stays in tier 1, so cards don't
/// jitter as agents cycle — only a meaningful transition (finishing → unread →
/// top, or stopping → bottom) moves a card between bands. Unread wins even for a
/// stopped session, so freshly-finished work floats up rather than sinking.
fn attention_tier(item: &SessionListItem) -> u8 {
    let SessionListItem::Worktree {
        status,
        agent_state,
        unread,
        ..
    } = item
    else {
        unreachable!("board rows are always Worktree")
    };
    if *unread
        || *status == SessionStatus::CascadePaused
        || matches!(agent_state, Some(AgentState::WaitingForInput))
    {
        0
    } else if *status == SessionStatus::Stopped {
        2
    } else {
        1
    }
}

/// Compute the display order of a set of sessions, grouping each stack directly
/// under its base.
///
/// Returns `(session_id, stacked_child)` pairs in display order. Root-list
/// sessions (unstacked + stack bases) are sorted newest-first by `created_at`;
/// stacked children follow their root in parent→child order at the single
/// deeper indent. Generic over [`SessionNode`] so both persisted sessions and
/// wire DTOs order identically.
pub fn build_session_order<S: SessionNode>(sessions: &[&S]) -> Vec<(SessionId, bool)> {
    let mut root_sessions: Vec<&S> = Vec::new();
    let mut children_by_parent: HashMap<SessionId, Vec<&S>> = HashMap::new();
    for s in sessions {
        match resolve_stack_parent(*s, sessions) {
            Some(parent_id) => {
                children_by_parent.entry(parent_id).or_default().push(s);
            }
            None => {
                root_sessions.push(s);
            }
        }
    }

    root_sessions.sort_by_key(|s| std::cmp::Reverse(s.node_created_at()));
    for children in children_by_parent.values_mut() {
        children.sort_by_key(|s| s.node_created_at());
    }

    let mut out = Vec::new();
    for root in root_sessions {
        out.push((root.node_id(), false));
        // to_visit is a LIFO stack; reverse the initial children and every
        // subsequent children-of-children push so pop() yields them in
        // ascending created_at order.
        let mut to_visit: Vec<&S> = children_by_parent
            .get(&root.node_id())
            .cloned()
            .unwrap_or_default();
        to_visit.reverse();
        while let Some(next) = to_visit.pop() {
            out.push((next.node_id(), true));
            if let Some(grandchildren) = children_by_parent.get(&next.node_id()) {
                for gc in grandchildren.iter().rev() {
                    to_visit.push(gc);
                }
            }
        }
    }
    out
}

/// Build a [`SessionListItem::Worktree`] row for a session DTO, optionally
/// prefixing the title with a project name and flagging it as a stacked child.
pub fn worktree_item(
    session: &SessionInfo,
    agent_states: &BTreeMap<SessionId, AgentState>,
    project_name_prefix: Option<&str>,
    stacked_child: bool,
) -> SessionListItem {
    let title = match project_name_prefix {
        Some(prefix) => format!("{}/{}", prefix, session.title),
        None => session.title.clone(),
    };
    SessionListItem::Worktree {
        id: session.session_id,
        project_id: session.project_id,
        title,
        branch: session.branch.clone(),
        status: session.status,
        program: session.program.clone(),
        pr_number: session.pr_number,
        pr_url: session.pr_url.clone(),
        pr_merged: session.pr_merged,
        // The DTO carries the already-effective PR state; the row renderer
        // re-applies `effective_pr_state`, which is idempotent on `Some`, so
        // wrapping preserves the previous rendering exactly.
        pr_state: Some(session.pr_state),
        pr_draft: session.pr_draft,
        pr_labels: session.pr_labels.clone(),
        worktree_path: std::path::PathBuf::from(&session.worktree_path),
        created_at: session.created_at,
        agent_state: agent_states.get(&session.session_id).copied(),
        unread: session.unread,
        keep_alive: session.keep_alive,
        // Set by refresh_list_items from UiState::lfs_pull_in_flight after the
        // items are built.
        lfs_pulling: false,
        stacked_child,
    }
}

/// Resolve the advisory WIP limit for a section by name. Returns the matching
/// `SectionConfig::max_sessions` for user-defined sections, the top-level
/// `in_progress_limit` for the implicit "In Progress" catch-all, or `None` when
/// no limit is configured.
pub fn resolve_section_limit(
    name: &str,
    sections: &[SectionConfig],
    in_progress_limit: Option<u32>,
) -> Option<u32> {
    if name == IN_PROGRESS {
        return in_progress_limit;
    }
    sections
        .iter()
        .find(|s| s.name == name)
        .and_then(|s| s.max_sessions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ProjectInfo, ServerStatus, Snapshot};
    use crate::backend::LOCAL_BACKEND_ID;
    use crate::session::{ProjectId, WorktreeSession};
    use chrono::{Duration as ChronoDuration, Utc};
    use std::path::PathBuf;

    fn make_session(title: &str, branch: &str, created_offset_secs: i64) -> WorktreeSession {
        let mut s = WorktreeSession::new(
            ProjectId::new(),
            title,
            branch,
            PathBuf::from("/tmp/wt"),
            "claude",
        );
        s.created_at = Utc::now() + ChronoDuration::seconds(created_offset_secs);
        s
    }

    fn make_session_in_section(
        title: &str,
        branch: &str,
        created_offset_secs: i64,
        current_section: &str,
    ) -> WorktreeSession {
        let mut s = make_session(title, branch, created_offset_secs);
        s.current_section = Some(current_section.to_string());
        // Stamp section-entry time to mirror the created offset so the leaf's
        // entered_section_at uniquely identifies the group's sort position.
        s.entered_section_at = Utc::now() + ChronoDuration::seconds(created_offset_secs);
        s
    }

    fn section_named(name: &str) -> SectionConfig {
        SectionConfig {
            name: name.to_string(),
            ..Default::default()
        }
    }

    /// Convert `WorktreeSession` fixtures into a one-server snapshot, deriving
    /// one project per distinct `project_id` (named `p-<id>` so name-sorting is
    /// deterministic per fixture set).
    fn snapshot_from(sessions: Vec<WorktreeSession>) -> Snapshot {
        let mut project_titles: HashMap<ProjectId, String> = Default::default();
        for s in &sessions {
            project_titles
                .entry(s.project_id)
                .or_insert_with(|| format!("p-{}", s.project_id));
        }
        let mut projects: Vec<ProjectInfo> = project_titles
            .into_iter()
            .map(|(pid, name)| ProjectInfo {
                id: pid,
                name,
                repo_path: PathBuf::from("/tmp"),
                main_branch: "main".to_string(),
                session_ids: sessions
                    .iter()
                    .filter(|s| s.project_id == pid)
                    .map(|s| s.id)
                    .collect(),
                origin_url: None,
                workspace: None,
            })
            .collect();
        projects.sort_by(|a, b| a.name.cmp(&b.name));
        let session_infos: Vec<crate::api::SessionInfo> = sessions
            .iter()
            .map(|s| {
                let pname = projects
                    .iter()
                    .find(|p| p.id == s.project_id)
                    .map(|p| p.name.as_str())
                    .unwrap_or("p");
                crate::api::session_info_from_session(s, pname)
            })
            .collect();
        Snapshot {
            projects,
            sessions: session_infos,
            cascade_paused: None,
            pending_comment_sessions: Vec::new(),
            project_pull: Default::default(),
            operations: Vec::new(),
            server: ServerStatus {
                gh_available: false,
                code_host: Default::default(),
                tmux_ok: true,
                version: "test".to_string(),
            },
            workspaces: Vec::new(),
            main_workspace: None,
            startup_workspace: Default::default(),
        }
    }

    /// Build a board from one local-backend snapshot with the given sections
    /// and agent states — the single-server shape most tests exercise.
    fn board_from(
        snapshot: &Snapshot,
        sections: &[SectionConfig],
        in_progress_limit: Option<u32>,
        agent_states: &BTreeMap<SessionId, AgentState>,
    ) -> Board {
        board_from_filtered(
            snapshot,
            sections,
            in_progress_limit,
            agent_states,
            None,
            false,
        )
    }

    /// As [`board_from`], with an active project filter and hide-empty toggle.
    fn board_from_filtered(
        snapshot: &Snapshot,
        sections: &[SectionConfig],
        in_progress_limit: Option<u32>,
        agent_states: &BTreeMap<SessionId, AgentState>,
        filter: Option<ProjectId>,
        hide_empty: bool,
    ) -> Board {
        build_board(
            &[BoardBackendInput {
                backend: LOCAL_BACKEND_ID,
                name: "local".to_string(),
                connection: ConnectionState::Connected,
                version_warning: None,
                snapshot,
                agent_states,
            }],
            sections,
            in_progress_limit,
            filter,
            hide_empty,
        )
    }

    /// Convenience: collect the session ids of a column's cards in row order.
    fn column_session_ids(board: &Board, column_name: &str) -> Vec<SessionId> {
        let column = board
            .columns
            .iter()
            .find(|c| c.name == column_name)
            .unwrap_or_else(|| panic!("column {column_name} present"));
        column
            .cards
            .iter()
            .map(|c| {
                let SessionListItem::Worktree { id, .. } = &c.row else {
                    unreachable!("board rows are always Worktree")
                };
                *id
            })
            .collect()
    }

    // --- build_session_order (moved from tui/app/state.rs) ---------------

    #[test]
    fn ordering_unstacked_only_sorts_newest_first() {
        let a = make_session("a", "a", 0);
        let b = make_session("b", "b", 10);
        let c = make_session("c", "c", 20);
        let order = build_session_order(&[&a, &b, &c]);
        assert_eq!(
            order,
            vec![(c.id, false), (b.id, false), (a.id, false)],
            "newer sessions should appear first at the root level"
        );
    }

    #[test]
    fn ordering_single_stack_emits_base_then_children_in_stack_order() {
        let base = make_session("base", "base-br", 0);
        let mut child1 = make_session("c1", "c1-br", 5);
        child1.stack_parent_session_id = Some(base.id);
        let mut child2 = make_session("c2", "c2-br", 10);
        child2.stack_parent_session_id = Some(child1.id);

        let order = build_session_order(&[&base, &child1, &child2]);
        assert_eq!(
            order,
            vec![(base.id, false), (child1.id, true), (child2.id, true)]
        );
    }

    #[test]
    fn ordering_two_independent_stacks_interleave_by_base_created_at() {
        let base_a = make_session("base-a", "base-a", 0);
        let mut child_a = make_session("child-a", "child-a", 1);
        child_a.stack_parent_session_id = Some(base_a.id);

        let base_b = make_session("base-b", "base-b", 20);
        let mut child_b = make_session("child-b", "child-b", 21);
        child_b.stack_parent_session_id = Some(base_b.id);

        let order = build_session_order(&[&base_a, &child_a, &base_b, &child_b]);
        assert_eq!(
            order,
            vec![
                (base_b.id, false),
                (child_b.id, true),
                (base_a.id, false),
                (child_a.id, true),
            ]
        );
    }

    #[test]
    fn ordering_mixed_stack_and_unstacked_interleaves_correctly() {
        let base = make_session("base", "base", 0);
        let mut child = make_session("child", "child", 5);
        child.stack_parent_session_id = Some(base.id);
        let solo = make_session("solo", "solo", 10);
        let order = build_session_order(&[&base, &child, &solo]);
        assert_eq!(
            order,
            vec![(solo.id, false), (base.id, false), (child.id, true)]
        );
    }

    #[test]
    fn ordering_orphan_stack_parent_is_treated_as_root() {
        let mut orphan = make_session("orphan", "orphan", 0);
        orphan.stack_parent_session_id = Some(SessionId::new());
        let order = build_session_order(&[&orphan]);
        assert_eq!(order, vec![(orphan.id, false)]);
    }

    #[test]
    fn ordering_sibling_children_of_same_base_both_indent() {
        let base = make_session("base", "base", 0);
        let mut c1 = make_session("c1", "c1", 5);
        c1.stack_parent_session_id = Some(base.id);
        let mut c2 = make_session("c2", "c2", 10);
        c2.stack_parent_session_id = Some(base.id);
        let order = build_session_order(&[&base, &c1, &c2]);
        assert_eq!(order, vec![(base.id, false), (c1.id, true), (c2.id, true)]);
    }

    #[test]
    fn ordering_pr_base_matching_session_forms_stack() {
        let base = make_session("base", "base-br", 0);
        let mut child = make_session("child", "child-br", 5);
        child.pr_base_branch = Some("base-br".to_string());
        let order = build_session_order(&[&base, &child]);
        assert_eq!(order, vec![(base.id, false), (child.id, true)]);
    }

    #[test]
    fn ordering_pr_base_matching_main_pops_child_to_root() {
        let base = make_session("base", "base-br", 0);
        let mut child = make_session("child", "child-br", 5);
        child.pr_base_branch = Some("main".to_string());
        child.stack_parent_session_id = Some(base.id);
        let order = build_session_order(&[&base, &child]);
        assert_eq!(
            order,
            vec![(child.id, false), (base.id, false)],
            "child with PR targeting main should pop to the root list"
        );
    }

    // --- resolve_section_limit (moved from tui/app/state.rs) -------------

    #[test]
    fn resolve_section_limit_uses_in_progress_limit_for_catch_all() {
        let sections = vec![section_named("Open")];
        assert_eq!(
            resolve_section_limit(IN_PROGRESS, &sections, Some(3)),
            Some(3)
        );
        assert_eq!(resolve_section_limit(IN_PROGRESS, &sections, None), None);
    }

    #[test]
    fn resolve_section_limit_reads_max_sessions_from_matching_config() {
        let sections = vec![
            section_named("Open"),
            SectionConfig {
                name: "Review".into(),
                max_sessions: Some(2),
                ..Default::default()
            },
        ];
        assert_eq!(resolve_section_limit("Review", &sections, None), Some(2));
        assert_eq!(resolve_section_limit("Open", &sections, None), None);
        assert_eq!(
            resolve_section_limit("Missing", &sections, Some(99)),
            None,
            "in_progress_limit must not leak into other section names"
        );
    }

    // --- build_board: columns --------------------------------------------

    #[test]
    fn hide_empty_drops_sectionless_columns_including_in_progress() {
        // A session that lands in "Open" leaves "In Progress" and "Done" empty.
        // With hide_empty, only the "Open" column survives.
        let mut s = make_session_in_section("s", "s", 0, "Open");
        s.pr_state = Some(crate::git::PrState::Open);
        let state = snapshot_from(vec![s]);
        let sections = vec![section_named("Open"), section_named("Done")];

        let hidden = board_from_filtered(&state, &sections, None, &BTreeMap::new(), None, true);
        let names: Vec<&str> = hidden.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Open"], "only the non-empty column remains");

        // The sidebar is untouched, and with hide_empty off every column shows.
        assert_eq!(hidden.projects.len(), 1);
        let shown = board_from_filtered(&state, &sections, None, &BTreeMap::new(), None, false);
        assert_eq!(
            shown.columns.len(),
            3,
            "all columns present when hide_empty is off"
        );
    }

    #[test]
    fn columns_are_in_progress_first_then_declared_order() {
        let state = snapshot_from(vec![]);
        let sections = vec![section_named("Alpha"), section_named("Beta")];
        let board = board_from(&state, &sections, None, &BTreeMap::new());
        let names: Vec<&str> = board.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec![IN_PROGRESS, "Alpha", "Beta"]);
    }

    #[test]
    fn default_sections_drive_columns_when_caller_passes_them() {
        // The board doesn't fall back to defaults itself — callers pass
        // effective_sections. With defaults, columns are In Progress / In
        // Review / Merged. With user sections, those replace the defaults.
        let state = snapshot_from(vec![]);
        let defaults = crate::session::effective_sections(&[]).into_owned();
        let board = board_from(&state, &defaults, None, &BTreeMap::new());
        let names: Vec<&str> = board.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec![IN_PROGRESS, "In Review", "Merged"]);

        let user = vec![section_named("My Column")];
        let effective = crate::session::effective_sections(&user).into_owned();
        let board = board_from(&state, &effective, None, &BTreeMap::new());
        let names: Vec<&str> = board.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec![IN_PROGRESS, "My Column"]);
    }

    #[test]
    fn in_progress_column_carries_in_progress_limit() {
        let state = snapshot_from(vec![]);
        let sections = vec![SectionConfig {
            name: "Review".into(),
            max_sessions: Some(4),
            ..Default::default()
        }];
        let board = board_from(&state, &sections, Some(2), &BTreeMap::new());
        assert_eq!(board.columns[0].name, IN_PROGRESS);
        assert_eq!(board.columns[0].max_sessions, Some(2));
        assert_eq!(board.columns[1].name, "Review");
        assert_eq!(board.columns[1].max_sessions, Some(4));
    }

    // --- build_board: sidebar --------------------------------------------

    #[test]
    fn sidebar_lists_all_projects_name_sorted_with_counts() {
        // Two projects; "Zeta" has two sessions, "Alpha" has one. Sidebar is
        // name-sorted regardless of insertion order.
        let zeta = ProjectId::new();
        let alpha = ProjectId::new();
        let mut s1 = make_session("z1", "z1", 0);
        s1.project_id = zeta;
        let mut s2 = make_session("z2", "z2", 1);
        s2.project_id = zeta;
        let mut s3 = make_session("a1", "a1", 2);
        s3.project_id = alpha;

        let mut state = snapshot_from(vec![s1, s2, s3]);
        // Rename the auto-generated project names to control sort order.
        for p in &mut state.projects {
            if p.id == zeta {
                p.name = "Zeta".into();
            } else if p.id == alpha {
                p.name = "Alpha".into();
            }
        }
        // Session DTOs and project list must agree on ordering inputs; the
        // builder re-sorts by name, so no further fixup is needed.

        let board = board_from(&state, &[], None, &BTreeMap::new());
        let entries: Vec<(&str, usize)> = board
            .projects
            .iter()
            .map(|e| (e.name.as_str(), e.session_count))
            .collect();
        assert_eq!(entries, vec![("Alpha", 1), ("Zeta", 2)]);
    }

    #[test]
    fn filter_shows_only_the_selected_projects_cards_but_full_sidebar() {
        // Two projects with sessions; filtering to one shows only its cards in
        // the columns, while the sidebar still lists both (each with its own
        // full session count).
        let keep = ProjectId::new();
        let hide = ProjectId::new();
        let mut k1 = make_session("k1", "k1", 0);
        k1.project_id = keep;
        let mut k2 = make_session("k2", "k2", 1);
        k2.project_id = keep;
        let mut h1 = make_session("h1", "h1", 2);
        h1.project_id = hide;
        let state = snapshot_from(vec![k1.clone(), k2.clone(), h1]);

        let board = board_from_filtered(&state, &[], None, &BTreeMap::new(), Some(keep), false);

        // Sidebar unaffected: both projects, real counts.
        assert_eq!(board.projects.len(), 2);
        assert!(board.projects.iter().any(|p| p.project_id == hide));
        let keep_entry = board
            .projects
            .iter()
            .find(|p| p.project_id == keep)
            .unwrap();
        assert_eq!(keep_entry.session_count, 2);

        // Columns hold only the kept project's cards.
        let card_projects: Vec<ProjectId> = board
            .columns
            .iter()
            .flat_map(|c| c.cards.iter())
            .map(|c| c.project_id)
            .collect();
        assert_eq!(card_projects.len(), 2, "only the kept project's two cards");
        assert!(card_projects.iter().all(|p| *p == keep));

        // Numbering renumbers over the visible cards only.
        assert_eq!(board.worktree_count(), 2);
        assert!(board.position_of(k1.id).is_some());
        assert!(board.position_of(k2.id).is_some());
    }

    #[test]
    fn filter_to_absent_project_yields_empty_columns_but_keeps_sidebar() {
        let a = ProjectId::new();
        let mut s = make_session("only", "only", 0);
        s.project_id = a;
        let state = snapshot_from(vec![s]);

        // Filter to a project that isn't in the snapshot → no cards, sidebar
        // still lists the real project.
        let board = board_from_filtered(
            &state,
            &[],
            None,
            &BTreeMap::new(),
            Some(ProjectId::new()),
            false,
        );
        assert_eq!(board.worktree_count(), 0);
        assert_eq!(board.projects.len(), 1);
        assert!(board.columns.iter().all(|c| c.cards.is_empty()));
    }

    #[test]
    fn empty_project_appears_in_sidebar_with_zero_count() {
        let mut state = snapshot_from(vec![]);
        state.projects.push(ProjectInfo {
            id: ProjectId::new(),
            name: "Empty".to_string(),
            repo_path: PathBuf::from("/tmp"),
            main_branch: "main".to_string(),
            session_ids: Vec::new(),
            origin_url: None,
            workspace: None,
        });

        let board = board_from(&state, &[], None, &BTreeMap::new());
        assert_eq!(board.projects.len(), 1);
        assert_eq!(board.projects[0].name, "Empty");
        assert_eq!(board.projects[0].session_count, 0);
        // ...and it contributes no cards to any column.
        assert!(board.columns.iter().all(|c| c.cards.is_empty()));
    }

    // --- build_board: stack-unit cards (ported scenarios) ----------------

    #[test]
    fn stack_lands_under_leaf_section_as_contiguous_cards() {
        let project_id = ProjectId::new();
        let mut base = make_session_in_section("base", "base", 0, "Review");
        base.project_id = project_id;
        let mut child = make_session_in_section("child", "child", 10, "Open");
        child.project_id = project_id;
        child.stack_parent_session_id = Some(base.id);

        let state = snapshot_from(vec![base.clone(), child.clone()]);
        let sections = vec![section_named("Open"), section_named("Review")];
        let board = board_from(&state, &sections, None, &BTreeMap::new());

        // The stack lands in Open (the leaf's section) as two contiguous cards:
        // the base unindented, its child indented, in stack order.
        let open = board.columns.iter().find(|c| c.name == "Open").unwrap();
        assert_eq!(open.cards.len(), 2);
        let cards: Vec<(SessionId, bool)> = open
            .cards
            .iter()
            .map(|c| {
                let SessionListItem::Worktree { id, .. } = &c.row else {
                    unreachable!("board rows are always Worktree")
                };
                (*id, c.indent)
            })
            .collect();
        assert_eq!(cards, vec![(base.id, false), (child.id, true)]);

        // Review is empty — the base followed the stack into Open.
        let review = board.columns.iter().find(|c| c.name == "Review").unwrap();
        assert!(review.cards.is_empty());
    }

    #[test]
    fn three_member_stack_yields_three_contiguous_cards_base_unindented() {
        let project_id = ProjectId::new();
        let mut base = make_session_in_section("base", "base", 0, "Open");
        base.project_id = project_id;
        let mut mid = make_session_in_section("mid", "mid", 10, "Open");
        mid.project_id = project_id;
        mid.stack_parent_session_id = Some(base.id);
        let mut top = make_session_in_section("top", "top", 20, "Open");
        top.project_id = project_id;
        top.stack_parent_session_id = Some(mid.id);

        let state = snapshot_from(vec![base.clone(), mid.clone(), top.clone()]);
        let sections = vec![section_named("Open")];
        let board = board_from(&state, &sections, None, &BTreeMap::new());

        let open = board.columns.iter().find(|c| c.name == "Open").unwrap();
        assert_eq!(open.cards.len(), 3, "one card per stack member");
        let cards: Vec<(SessionId, bool)> = open
            .cards
            .iter()
            .map(|c| {
                let SessionListItem::Worktree { id, .. } = &c.row else {
                    unreachable!("board rows are always Worktree")
                };
                (*id, c.indent)
            })
            .collect();
        assert_eq!(
            cards,
            vec![(base.id, false), (mid.id, true), (top.id, true)],
            "base unindented, children indented, contiguous in stack order"
        );
    }

    #[test]
    fn override_closest_to_leaf_wins() {
        let project_id = ProjectId::new();
        let mut base = make_session_in_section("base", "base", 0, "Review");
        base.project_id = project_id;
        base.section_override = Some("Pinned".to_string());
        let mut child = make_session_in_section("child", "child", 10, "Open");
        child.project_id = project_id;
        child.stack_parent_session_id = Some(base.id);

        let state = snapshot_from(vec![base.clone(), child.clone()]);
        let sections = vec![
            section_named("Open"),
            section_named("Review"),
            section_named("Pinned"),
        ];
        let board = board_from(&state, &sections, None, &BTreeMap::new());

        assert_eq!(
            column_session_ids(&board, "Pinned"),
            vec![base.id, child.id]
        );
        assert!(column_session_ids(&board, "Open").is_empty());
    }

    #[test]
    fn stale_override_falls_back_to_current_section() {
        let project_id = ProjectId::new();
        let mut s = make_session_in_section("s", "s", 0, "Open");
        s.project_id = project_id;
        s.section_override = Some("Deleted Section".to_string());

        let state = snapshot_from(vec![s.clone()]);
        let sections = vec![section_named("Open"), section_named("Review")];
        let board = board_from(&state, &sections, None, &BTreeMap::new());

        assert_eq!(column_session_ids(&board, "Open"), vec![s.id]);
    }

    #[test]
    fn fan_out_group_shares_root_and_uses_newest_leaf() {
        let project_id = ProjectId::new();
        let mut base = make_session_in_section("base", "base", 0, "Review");
        base.project_id = project_id;
        let mut b = make_session_in_section("b", "b", 5, "Review");
        b.project_id = project_id;
        b.stack_parent_session_id = Some(base.id);
        let mut c = make_session_in_section("c", "c", 20, "Open");
        c.project_id = project_id;
        c.stack_parent_session_id = Some(base.id);

        let state = snapshot_from(vec![base.clone(), b.clone(), c.clone()]);
        let sections = vec![section_named("Open"), section_named("Review")];
        let board = board_from(&state, &sections, None, &BTreeMap::new());

        let open_ids = column_session_ids(&board, "Open");
        assert!(
            open_ids.contains(&base.id) && open_ids.contains(&b.id) && open_ids.contains(&c.id),
            "all three subgraph members should appear under Open: {open_ids:?}"
        );
        assert!(column_session_ids(&board, "Review").is_empty());
    }

    #[test]
    fn sibling_override_off_leaf_path_is_ignored() {
        let project_id = ProjectId::new();
        let mut base = make_session_in_section("base", "base", 0, "Review");
        base.project_id = project_id;
        let mut b = make_session_in_section("b", "b", 5, "Review");
        b.project_id = project_id;
        b.stack_parent_session_id = Some(base.id);
        b.section_override = Some("Pinned".to_string());
        let mut c = make_session_in_section("c", "c", 20, "Open");
        c.project_id = project_id;
        c.stack_parent_session_id = Some(base.id);

        let state = snapshot_from(vec![base.clone(), b.clone(), c.clone()]);
        let sections = vec![
            section_named("Open"),
            section_named("Review"),
            section_named("Pinned"),
        ];
        let board = board_from(&state, &sections, None, &BTreeMap::new());

        assert!(column_session_ids(&board, "Pinned").is_empty());
        assert_eq!(column_session_ids(&board, "Open").len(), 3);
    }

    #[test]
    fn cards_sort_newest_first_within_a_tier() {
        // Two same-tier (running/idle) sessions: the newer one floats above the
        // older, so recent work is easy to find at the top of the band.
        let project_id = ProjectId::new();
        let mut older = make_session_in_section("older", "older", 0, "Open");
        older.project_id = project_id;
        let mut newer = make_session_in_section("newer", "newer", 20, "Open");
        newer.project_id = project_id;

        let state = snapshot_from(vec![older.clone(), newer.clone()]);
        let sections = vec![section_named("Open")];
        let board = board_from(&state, &sections, None, &BTreeMap::new());

        assert_eq!(
            column_session_ids(&board, "Open"),
            vec![newer.id, older.id],
            "within a tier, newer sessions sort first"
        );
    }

    #[test]
    fn cards_sort_by_attention_tier_needs_you_top_stopped_bottom() {
        // A column with one session per band: unread (needs you), a plain
        // running session (active), and a stopped one. They sort top-to-bottom
        // by attention regardless of age.
        let project_id = ProjectId::new();
        // `stopped` is the NEWEST by created_at, to prove tier beats recency.
        let mut unread = make_session("unread", "unread", 0);
        unread.project_id = project_id;
        unread.unread = true;
        let mut active = make_session("active", "active", 10);
        active.project_id = project_id;
        let mut stopped = make_session("stopped", "stopped", 20);
        stopped.project_id = project_id;
        stopped.status = SessionStatus::Stopped;

        let state = snapshot_from(vec![unread.clone(), active.clone(), stopped.clone()]);
        let board = board_from(&state, &[], None, &BTreeMap::new());

        assert_eq!(
            column_session_ids(&board, IN_PROGRESS),
            vec![unread.id, active.id, stopped.id],
            "needs-you (unread) at the top, active in the middle, stopped at the bottom"
        );
    }

    #[test]
    fn waiting_for_input_floats_to_the_top_tier() {
        // An agent blocked on input needs the user, so it sits in the top band
        // above a plain running session even when the running one is newer.
        let project_id = ProjectId::new();
        let mut waiting = make_session("waiting", "waiting", 0);
        waiting.project_id = project_id;
        let mut running = make_session("running", "running", 20);
        running.project_id = project_id;

        let state = snapshot_from(vec![waiting.clone(), running.clone()]);
        let mut agent_states = BTreeMap::new();
        agent_states.insert(waiting.id, AgentState::WaitingForInput);
        let board = board_from(&state, &[], None, &agent_states);

        assert_eq!(
            column_session_ids(&board, IN_PROGRESS),
            vec![waiting.id, running.id],
            "a waiting-for-input session outranks a newer plain-running one"
        );
    }

    #[test]
    fn build_board_is_deterministic_across_repeated_calls() {
        // Two stacks whose leaves entered the section at the same instant — a
        // realistic outcome of one batched apply_assignment pass. Output must
        // not depend on HashMap iteration order.
        let project_id = ProjectId::new();
        let same_ts = Utc::now();
        let mk = |title: &str, branch: &str| {
            let mut s = make_session_in_section(title, branch, 0, "Open");
            s.project_id = project_id;
            s.entered_section_at = same_ts;
            s.created_at = same_ts;
            s
        };
        let base_a = mk("base-a", "base-a");
        let mut child_a = mk("child-a", "child-a");
        child_a.stack_parent_session_id = Some(base_a.id);
        let base_b = mk("base-b", "base-b");
        let mut child_b = mk("child-b", "child-b");
        child_b.stack_parent_session_id = Some(base_b.id);

        let state = snapshot_from(vec![
            base_a.clone(),
            child_a.clone(),
            base_b.clone(),
            child_b.clone(),
        ]);
        let sections = vec![section_named("Open")];

        let first = board_from(&state, &sections, None, &BTreeMap::new());
        for _ in 0..32 {
            let again = board_from(&state, &sections, None, &BTreeMap::new());
            assert_eq!(again, first, "build_board must be deterministic");
        }
    }

    // --- numbering + position_of / ids_at --------------------------------

    #[test]
    fn session_numbers_and_pos_of_number_are_column_major_and_round_trip() {
        // One project, three sessions: two in In Progress, one in Open. Numbers
        // run column-major (In Progress rows first), and pos_of_session_number
        // is the exact inverse of session_numbers via position_of.
        let project_id = ProjectId::new();
        let mut a = make_session("a", "a", 0);
        a.project_id = project_id;
        let mut b = make_session("b", "b", 5);
        b.project_id = project_id;
        let mut open = make_session_in_section("open", "open", 10, "Open");
        open.project_id = project_id;

        let state = snapshot_from(vec![a.clone(), b.clone(), open.clone()]);
        let sections = vec![section_named("Open")];
        let board = board_from(&state, &sections, None, &BTreeMap::new());

        let numbers = board.session_numbers();
        assert_eq!(numbers.len(), 3);
        // The Open-column session is numbered last (column-major).
        assert_eq!(numbers[&open.id], 3);

        for (&id, &n) in &numbers {
            assert_eq!(
                board.pos_of_session_number(n),
                board.position_of(id),
                "pos_of_session_number(n) must match position_of(id) for number n"
            );
        }
        // Out-of-range numbers (and 0) yield nothing.
        assert_eq!(board.pos_of_session_number(0), None);
        assert_eq!(board.pos_of_session_number(4), None);
        assert_eq!(board.worktree_count(), 3);
    }

    #[test]
    fn selectable_row_counts_agree_with_ids_at() {
        // counts[0] = sidebar; counts[col] = number of rows addressable by
        // ids_at in that column. Cross-check the two so BoardState::sync and
        // the board agree on selectable geometry.
        let project_id = ProjectId::new();
        let mut a = make_session("a", "a", 0);
        a.project_id = project_id;
        let mut open = make_session_in_section("open", "open", 10, "Open");
        open.project_id = project_id;
        let state = snapshot_from(vec![a, open]);
        let sections = vec![section_named("Open")];
        let board = board_from(&state, &sections, None, &BTreeMap::new());

        let counts = board.selectable_row_counts();
        assert_eq!(counts[0], board.projects.len());
        for (col_idx, &count) in counts.iter().enumerate().skip(1) {
            for row in 0..count {
                let (sid, _) = board.ids_at(BoardPos { col: col_idx, row });
                assert!(
                    sid.is_some(),
                    "every row within selectable_row_counts must resolve to a session"
                );
            }
            // One past the count resolves to nothing.
            assert_eq!(
                board.ids_at(BoardPos {
                    col: col_idx,
                    row: count
                }),
                (None, None)
            );
        }
    }

    #[test]
    fn position_of_and_ids_at_round_trip() {
        let project_id = ProjectId::new();
        let mut base = make_session_in_section("base", "base", 0, "Open");
        base.project_id = project_id;
        let mut child = make_session_in_section("child", "child", 5, "Open");
        child.project_id = project_id;
        child.stack_parent_session_id = Some(base.id);
        let mut wip = make_session("wip", "wip", 10);
        wip.project_id = project_id;

        let state = snapshot_from(vec![base.clone(), child.clone(), wip.clone()]);
        let sections = vec![section_named("Open")];
        let board = board_from(&state, &sections, None, &BTreeMap::new());

        for id in [base.id, child.id, wip.id] {
            let pos = board.position_of(id).expect("session on board");
            let (sid, pid) = board.ids_at(pos);
            assert_eq!(sid, Some(id), "ids_at should recover the session");
            assert_eq!(pid, Some(project_id));
        }

        // wip is in the In Progress column (col 1); base/child in Open (col 2).
        assert_eq!(board.position_of(wip.id).unwrap().col, 1);
        assert_eq!(board.position_of(base.id).unwrap().col, 2);
        assert_eq!(
            board.position_of(child.id).unwrap(),
            BoardPos { col: 2, row: 1 }
        );
    }

    #[test]
    fn ids_at_sidebar_row_yields_project_only() {
        let project_id = ProjectId::new();
        let mut s = make_session("s", "s", 0);
        s.project_id = project_id;
        let state = snapshot_from(vec![s]);
        let board = board_from(&state, &[], None, &BTreeMap::new());

        let (sid, pid) = board.ids_at(BoardPos { col: 0, row: 0 });
        assert_eq!(sid, None);
        assert_eq!(pid, Some(project_id));
    }

    #[test]
    fn ids_at_out_of_range_yields_nothing() {
        let state = snapshot_from(vec![]);
        let board = board_from(&state, &[], None, &BTreeMap::new());
        // Empty board: no sidebar rows, In Progress column has no rows.
        assert_eq!(board.ids_at(BoardPos { col: 0, row: 0 }), (None, None));
        assert_eq!(board.ids_at(BoardPos { col: 1, row: 0 }), (None, None));
        assert_eq!(board.ids_at(BoardPos { col: 99, row: 0 }), (None, None));
    }

    #[test]
    fn position_of_returns_none_for_absent_session() {
        let state = snapshot_from(vec![]);
        let board = board_from(&state, &[], None, &BTreeMap::new());
        assert_eq!(board.position_of(SessionId::new()), None);
    }
}

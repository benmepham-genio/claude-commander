//! Workspace decisions every frontend makes about snapshots it already holds:
//! which workspaces exist across all connected servers, which projects and
//! sessions the active one shows, how many sessions are waiting elsewhere, and
//! where a frontend opens.
//!
//! A workspace is a label on a project (`ProjectInfo::workspace`); `None` is the
//! built-in **Main**. Workspace *identity* is the name, so Main is represented
//! by `None` here too — [`MergedWorkspace::name`] is `None` for Main and every
//! `active: Option<&str>` parameter reads `None` as Main.
//!
//! Shared rather than per-frontend because the TUI and the Flutter app must
//! agree on the merged order and on what "in this workspace" means, or the same
//! fleet looks different on each.

use claude_commander_protocol::api::{AgentStatesSnapshot, ProjectInfo, SessionInfo, Snapshot};
use claude_commander_protocol::session::{AgentState, ProjectId};
use claude_commander_protocol::workspace::{MAIN_WORKSPACE_LABEL, StartupWorkspace, WorkspaceDef};

/// One workspace in the merged, cross-server list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedWorkspace {
    /// The workspace's identity: `None` for Main, else the tag projects carry.
    pub name: Option<String>,
    /// What to display. Main's comes from the first server that renamed it;
    /// a named workspace's label is its name.
    pub label: String,
}

impl MergedWorkspace {
    pub fn is_main(&self) -> bool {
        self.name.is_none()
    }
}

/// What one server contributes to the merge. Borrowed so a frontend can build
/// it from whatever it holds; [`WorkspaceSource::of`] builds it from a
/// [`Snapshot`].
#[derive(Debug, Clone, Default)]
pub struct WorkspaceSource<'a> {
    /// The server's definitions, in its configured order.
    pub defs: &'a [WorkspaceDef],
    /// The server's Main label (`None` = default label).
    pub main: Option<&'a WorkspaceDef>,
    /// Workspace tags on the server's projects, in project order. Tags with no
    /// definition anywhere still get a workspace, so no project can become
    /// unreachable because a definition was lost (an older binary rewrote
    /// `config.toml`, or a stale client replaced the list).
    pub project_tags: Vec<&'a str>,
}

impl<'a> WorkspaceSource<'a> {
    pub fn of(snapshot: &'a Snapshot) -> Self {
        Self {
            defs: &snapshot.workspaces,
            main: snapshot.main_workspace.as_ref(),
            project_tags: snapshot
                .projects
                .iter()
                .filter_map(|p| p.workspace.as_deref())
                .collect(),
        }
    }
}

/// Merge workspace definitions across servers **by name**.
///
/// - Main is always first. Its label comes from the first source that sets
///   one, else [`MAIN_WORKSPACE_LABEL`].
/// - Named workspaces follow in the first source's order; names only later
///   sources define are appended in the order they appear.
/// - Tags used by projects but defined nowhere are appended last.
pub fn merge_workspace_sources(sources: &[WorkspaceSource<'_>]) -> Vec<MergedWorkspace> {
    let main = sources.iter().find_map(|s| s.main);
    let mut merged = vec![MergedWorkspace {
        name: None,
        label: main.map_or_else(|| MAIN_WORKSPACE_LABEL.to_string(), |m| m.name.clone()),
    }];

    let defined = sources
        .iter()
        .flat_map(|s| s.defs.iter().map(|d| d.name.as_str()));
    let tagged = sources.iter().flat_map(|s| s.project_tags.iter().copied());
    for name in defined.chain(tagged) {
        if !merged.iter().any(|m| m.name.as_deref() == Some(name)) {
            merged.push(MergedWorkspace {
                name: Some(name.to_string()),
                label: name.to_string(),
            });
        }
    }
    merged
}

/// [`merge_workspace_sources`] over whole snapshots, in backend order (the TUI's
/// local server first; the Flutter client's first server first).
pub fn merge_workspaces<'a>(
    snapshots: impl IntoIterator<Item = &'a Snapshot>,
) -> Vec<MergedWorkspace> {
    let sources: Vec<WorkspaceSource<'a>> =
        snapshots.into_iter().map(WorkspaceSource::of).collect();
    merge_workspace_sources(&sources)
}

/// Whether frontends should show workspace UI at all: only once a second
/// workspace exists.
pub fn workspaces_visible(workspaces: &[MergedWorkspace]) -> bool {
    workspaces.len() >= 2
}

/// Whether a project tagged `project_workspace` belongs to `active`
/// (`None` = Main on both sides).
pub fn in_workspace(project_workspace: Option<&str>, active: Option<&str>) -> bool {
    project_workspace == active
}

/// The snapshot's projects in `active`, in snapshot order.
pub fn projects_in_workspace<'a>(
    snapshot: &'a Snapshot,
    active: Option<&str>,
) -> Vec<&'a ProjectInfo> {
    snapshot
        .projects
        .iter()
        .filter(|p| in_workspace(p.workspace.as_deref(), active))
        .collect()
}

/// The workspace a session belongs to: its project's tag. A session whose
/// project is missing from the snapshot reads as Main, so it stays visible
/// somewhere rather than vanishing.
pub fn session_workspace<'a>(snapshot: &'a Snapshot, session: &SessionInfo) -> Option<&'a str> {
    project_workspace(snapshot, session.project_id)
}

/// The tag on project `id` (`None` for Main or an unknown project).
pub fn project_workspace(snapshot: &Snapshot, id: ProjectId) -> Option<&str> {
    snapshot
        .projects
        .iter()
        .find(|p| p.id == id)
        .and_then(|p| p.workspace.as_deref())
}

/// The snapshot's sessions in `active`, in snapshot order.
pub fn sessions_in_workspace<'a>(
    snapshot: &'a Snapshot,
    active: Option<&str>,
) -> Vec<&'a SessionInfo> {
    snapshot
        .sessions
        .iter()
        .filter(|s| in_workspace(session_workspace(snapshot, s), active))
        .collect()
}

/// How many sessions in one workspace are waiting for input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceWaiting {
    /// `None` for Main.
    pub name: Option<String>,
    pub waiting: usize,
}

/// Sessions waiting for input ([`AgentState::WaitingForInput`]) per workspace,
/// summed across backends — one entry per element of `workspaces`, in the same
/// order (zeros included). Only real sessions count: the commander's sentinel
/// entry in `states` has no session and is never seen.
pub fn waiting_counts<'a>(
    workspaces: &[MergedWorkspace],
    backends: impl IntoIterator<Item = (&'a Snapshot, &'a AgentStatesSnapshot)>,
) -> Vec<WorkspaceWaiting> {
    let mut counts: Vec<WorkspaceWaiting> = workspaces
        .iter()
        .map(|w| WorkspaceWaiting {
            name: w.name.clone(),
            waiting: 0,
        })
        .collect();
    for (snapshot, states) in backends {
        for session in &snapshot.sessions {
            if states.states.get(&session.session_id) != Some(&AgentState::WaitingForInput) {
                continue;
            }
            let ws = session_workspace(snapshot, session);
            if let Some(entry) = counts.iter_mut().find(|c| c.name.as_deref() == ws) {
                entry.waiting += 1;
            }
        }
    }
    counts
}

/// The non-zero counts for every workspace except `active` — the status bar's
/// "others are waiting" hints.
pub fn waiting_elsewhere<'c>(
    counts: &'c [WorkspaceWaiting],
    active: Option<&str>,
) -> Vec<&'c WorkspaceWaiting> {
    counts
        .iter()
        .filter(|c| c.waiting > 0 && c.name.as_deref() != active)
        .collect()
}

/// The workspace a frontend asks for at startup, before any snapshot is
/// known: `last` is the client's remembered active workspace (`None` = Main or
/// never set). Kept as asked — existence is [`effective_workspace`]'s job — so a
/// pinned workspace applies as soon as the snapshot carrying it arrives.
pub fn requested_startup_workspace(
    startup: &StartupWorkspace,
    last: Option<&str>,
) -> Option<String> {
    match startup {
        StartupWorkspace::Main => None,
        StartupWorkspace::Last => last.map(str::to_string),
        StartupWorkspace::Named(name) => Some(name.clone()),
    }
}

/// The workspace actually shown for a `requested` one: itself while some
/// server still has it, else Main (`None`).
pub fn effective_workspace(
    requested: Option<&str>,
    workspaces: &[MergedWorkspace],
) -> Option<String> {
    let wanted = requested?;
    workspaces
        .iter()
        .any(|w| w.name.as_deref() == Some(wanted))
        .then(|| wanted.to_string())
}

/// The workspace a frontend opens on: [`requested_startup_workspace`] checked
/// by [`effective_workspace`]. A pinned or remembered workspace that no longer
/// exists falls back to Main (`None`).
pub fn resolve_startup_workspace(
    startup: &StartupWorkspace,
    last: Option<&str>,
    workspaces: &[MergedWorkspace],
) -> Option<String> {
    effective_workspace(
        requested_startup_workspace(startup, last).as_deref(),
        workspaces,
    )
}

/// Whether `name` would collide with an existing workspace — a name, or Main's
/// label — compared trimmed and case-insensitively, as a server's
/// `validate_set_workspaces` compares them. `except` is the workspace being
/// renamed, which may keep (or re-case) its own name.
pub fn workspace_name_taken(
    workspaces: &[MergedWorkspace],
    name: &str,
    except: Option<&MergedWorkspace>,
) -> bool {
    let name = name.trim();
    workspaces
        .iter()
        .filter(|w| Some(*w) != except)
        .any(|w| w.label.eq_ignore_ascii_case(name))
}

/// Narrow `wanted` — the definition list an edit produced from the merged
/// view — to a list one server will accept, given the server's `own`
/// definitions and the Main label it will have (`main_label`: the one being
/// sent, else its stored one).
///
/// The merge is exact-name and keeps Main apart from a same-named definition,
/// but a server refuses any case-insensitive clash. So once two servers
/// disagree (each created "Work"/"work" while the other was unreachable), the
/// merged list is valid for neither. Per server:
///
/// - Every entry the server already defines by that exact name is kept — never
///   silently dropped, since that would orphan its projects' tags. If its own
///   list is already inconsistent the server refuses, and the frontend says so.
/// - Every other entry is kept unless it clashes, case-insensitively, with
///   `main_label`, one of the server's own names, or an entry already kept.
///
/// Order follows `wanted`.
pub fn definitions_for_server(
    wanted: &[WorkspaceDef],
    own: &[WorkspaceDef],
    main_label: Option<&str>,
) -> Vec<WorkspaceDef> {
    let is_own = |d: &WorkspaceDef| own.iter().any(|o| o.name == d.name);
    let mut claimed: Vec<&str> = main_label.into_iter().collect();
    claimed.extend(own.iter().map(|o| o.name.as_str()));
    let mut out = Vec::with_capacity(wanted.len());
    for def in wanted {
        if is_own(def) {
            out.push(def.clone());
            continue;
        }
        if claimed.iter().any(|c| c.eq_ignore_ascii_case(&def.name)) {
            continue;
        }
        claimed.push(&def.name);
        out.push(def.clone());
    }
    out
}

/// The workspace after (or, with `forward = false`, before) `active`, wrapping.
/// An `active` not in the list starts from Main.
pub fn cycle_workspace(
    workspaces: &[MergedWorkspace],
    active: Option<&str>,
    forward: bool,
) -> Option<String> {
    let len = workspaces.len();
    if len == 0 {
        return None;
    }
    let idx = workspaces
        .iter()
        .position(|w| w.name.as_deref() == active)
        .unwrap_or(0);
    let next = if forward {
        (idx + 1) % len
    } else {
        (idx + len - 1) % len
    };
    workspaces[next].name.clone()
}

/// Resolve what an operator typed (a CLI `--workspace` value, say) to a
/// workspace: an exact name first, then a case-insensitive name, then Main by
/// its label or the word `main`. `None` when nothing matches.
pub fn find_workspace<'w>(
    workspaces: &'w [MergedWorkspace],
    typed: &str,
) -> Option<&'w MergedWorkspace> {
    let typed = typed.trim();
    workspaces
        .iter()
        .find(|w| w.name.as_deref() == Some(typed))
        .or_else(|| {
            workspaces.iter().find(|w| {
                w.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(typed))
            })
        })
        .or_else(|| {
            workspaces.iter().find(|w| {
                w.is_main()
                    && (w.label.eq_ignore_ascii_case(typed) || typed.eq_ignore_ascii_case("main"))
            })
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use claude_commander_protocol::api::ServerStatus;
    use claude_commander_protocol::session::{SessionId, SessionStatus};

    use super::*;

    fn def(name: &str) -> WorkspaceDef {
        WorkspaceDef::named(name)
    }

    fn project(name: &str, workspace: Option<&str>) -> ProjectInfo {
        ProjectInfo {
            id: ProjectId::new(),
            name: name.to_string(),
            repo_path: PathBuf::from(format!("/{name}")),
            main_branch: "main".to_string(),
            session_ids: Vec::new(),
            origin_url: None,
            workspace: workspace.map(str::to_string),
        }
    }

    fn session(project: &ProjectInfo, title: &str) -> SessionInfo {
        let sid = SessionId::new();
        serde_json::from_value(serde_json::json!({
            "id": sid.to_string(),
            "session_id": sid,
            "title": title,
            "branch": title,
            "status": SessionStatus::Running,
            "program": "claude",
            "project_id": project.id,
            "project_name": project.name,
            "pr_number": null,
            "pr_url": null,
            "pr_state": "open",
            "pr_draft": false,
            "pr_labels": [],
            "review_decision": null,
            "pr_reviewers": [],
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap()
    }

    fn snapshot(
        projects: Vec<ProjectInfo>,
        sessions: Vec<SessionInfo>,
        workspaces: Vec<WorkspaceDef>,
        main: Option<WorkspaceDef>,
    ) -> Snapshot {
        Snapshot {
            projects,
            sessions,
            cascade_paused: None,
            pending_comment_sessions: Vec::new(),
            project_pull: BTreeMap::new(),
            operations: Vec::new(),
            server: ServerStatus {
                code_host: Default::default(),
                gh_available: false,
                tmux_ok: true,
                version: "0".to_string(),
            },
            workspaces,
            main_workspace: main,
            startup_workspace: StartupWorkspace::Last,
        }
    }

    fn names(ws: &[MergedWorkspace]) -> Vec<Option<&str>> {
        ws.iter().map(|w| w.name.as_deref()).collect()
    }

    #[test]
    fn a_server_with_no_definitions_has_only_main() {
        let merged = merge_workspaces([&snapshot(vec![], vec![], vec![], None)]);
        assert_eq!(names(&merged), [None]);
        assert_eq!(merged[0].label, MAIN_WORKSPACE_LABEL);
        assert!(!workspaces_visible(&merged));
    }

    #[test]
    fn merge_keeps_the_first_servers_order_and_appends_extras() {
        let a = snapshot(vec![], vec![], vec![def("Work"), def("Play")], None);
        let b = snapshot(
            vec![],
            vec![],
            vec![def("Oss"), def("Play"), def("Work")],
            None,
        );
        let merged = merge_workspaces([&a, &b]);
        assert_eq!(
            names(&merged),
            [None, Some("Work"), Some("Play"), Some("Oss")]
        );
        assert!(workspaces_visible(&merged));
    }

    #[test]
    fn mains_label_comes_from_the_first_server_that_sets_one() {
        let old = snapshot(vec![], vec![], vec![], None);
        let home = snapshot(vec![], vec![], vec![], Some(def("Home")));
        let other = snapshot(vec![], vec![], vec![], Some(def("Base")));
        let merged = merge_workspaces([&old, &home, &other]);
        assert_eq!(merged[0].label, "Home");
        assert!(merged[0].is_main());
    }

    /// Main merges by being untagged, never by name — a server that labels
    /// Main "Work" and one that defines a workspace "Work" are two workspaces.
    #[test]
    fn main_never_merges_with_a_workspace_of_the_same_label() {
        let a = snapshot(vec![], vec![], vec![], Some(def("Work")));
        let b = snapshot(vec![], vec![], vec![def("Work")], None);
        let merged = merge_workspaces([&a, &b]);
        assert_eq!(names(&merged), [None, Some("Work")]);
    }

    #[test]
    fn an_undefined_tag_still_gets_a_workspace() {
        let s = snapshot(
            vec![project("a", Some("Lost")), project("b", Some("Work"))],
            vec![],
            vec![def("Work")],
            None,
        );
        let merged = merge_workspaces([&s]);
        assert_eq!(names(&merged), [None, Some("Work"), Some("Lost")]);
    }

    #[test]
    fn filters_projects_and_sessions_by_the_active_workspace() {
        let main_p = project("main-repo", None);
        let work_p = project("work-repo", Some("Work"));
        let s1 = session(&main_p, "one");
        let s2 = session(&work_p, "two");
        let snap = snapshot(
            vec![main_p.clone(), work_p.clone()],
            vec![s1.clone(), s2.clone()],
            vec![def("Work")],
            None,
        );
        let main_projects: Vec<_> = projects_in_workspace(&snap, None)
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(main_projects, [main_p.id]);
        let work_sessions: Vec<_> = sessions_in_workspace(&snap, Some("Work"))
            .iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(work_sessions, [s2.session_id]);
        assert!(sessions_in_workspace(&snap, Some("Nope")).is_empty());
        assert_eq!(session_workspace(&snap, &s1), None);
    }

    #[test]
    fn counts_waiting_sessions_per_workspace_across_backends() {
        let work_a = project("a", Some("Work"));
        let main_a = project("m", None);
        let wa = session(&work_a, "wa");
        let wb = session(&work_a, "wb");
        let ma = session(&main_a, "ma");
        let a = snapshot(
            vec![work_a, main_a],
            vec![wa.clone(), wb.clone(), ma.clone()],
            vec![def("Work")],
            None,
        );
        let mut a_states = BTreeMap::new();
        a_states.insert(wa.session_id, AgentState::WaitingForInput);
        a_states.insert(wb.session_id, AgentState::Working);
        a_states.insert(ma.session_id, AgentState::WaitingForInput);
        // The commander sentinel has no session, so it can never count.
        a_states.insert(SessionId::new(), AgentState::WaitingForInput);
        let a_agents = AgentStatesSnapshot {
            states: a_states,
            commander_running: true,
        };

        let work_b = project("b", Some("Work"));
        let wc = session(&work_b, "wc");
        let b = snapshot(vec![work_b], vec![wc.clone()], vec![], None);
        let b_agents = AgentStatesSnapshot {
            states: BTreeMap::from([(wc.session_id, AgentState::WaitingForInput)]),
            commander_running: false,
        };

        let merged = merge_workspaces([&a, &b]);
        let counts = waiting_counts(&merged, [(&a, &a_agents), (&b, &b_agents)]);
        assert_eq!(
            counts,
            [
                WorkspaceWaiting {
                    name: None,
                    waiting: 1
                },
                WorkspaceWaiting {
                    name: Some("Work".into()),
                    waiting: 2
                },
            ]
        );
        let elsewhere: Vec<_> = waiting_elsewhere(&counts, Some("Work"))
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(elsewhere, [None]);
    }

    #[test]
    fn startup_resolves_last_or_pinned_and_falls_back_to_main() {
        let merged = merge_workspaces([&snapshot(vec![], vec![], vec![def("Work")], None)]);
        let last = StartupWorkspace::Last;
        assert_eq!(
            resolve_startup_workspace(&last, Some("Work"), &merged).as_deref(),
            Some("Work")
        );
        assert_eq!(resolve_startup_workspace(&last, None, &merged), None);
        assert_eq!(
            resolve_startup_workspace(&last, Some("Gone"), &merged),
            None
        );
        let pinned = StartupWorkspace::Named("Work".into());
        assert_eq!(
            resolve_startup_workspace(&pinned, None, &merged).as_deref(),
            Some("Work")
        );
        let missing = StartupWorkspace::Named("Gone".into());
        assert_eq!(
            resolve_startup_workspace(&missing, Some("Work"), &merged),
            None
        );
        assert_eq!(
            resolve_startup_workspace(&StartupWorkspace::Main, Some("Work"), &merged),
            None
        );
    }

    #[test]
    fn cycling_wraps_both_ways() {
        let merged = merge_workspaces([&snapshot(
            vec![],
            vec![],
            vec![def("Work"), def("Play")],
            None,
        )]);
        assert_eq!(
            cycle_workspace(&merged, None, true).as_deref(),
            Some("Work")
        );
        assert_eq!(cycle_workspace(&merged, Some("Play"), true), None);
        assert_eq!(
            cycle_workspace(&merged, None, false).as_deref(),
            Some("Play")
        );
        assert_eq!(
            cycle_workspace(&merged, Some("Gone"), true).as_deref(),
            Some("Work")
        );
        assert_eq!(cycle_workspace(&[], None, true), None);
    }

    #[test]
    fn effective_workspace_needs_the_request_to_exist() {
        let merged = merge_workspaces([&snapshot(vec![], vec![], vec![def("Work")], None)]);
        assert_eq!(
            effective_workspace(Some("Work"), &merged).as_deref(),
            Some("Work")
        );
        assert_eq!(effective_workspace(Some("Gone"), &merged), None);
        assert_eq!(effective_workspace(None, &merged), None);
        assert_eq!(
            requested_startup_workspace(&StartupWorkspace::Named("Home".into()), Some("Work"))
                .as_deref(),
            Some("Home"),
            "the request is kept as asked, before any snapshot confirms it"
        );
        assert_eq!(
            requested_startup_workspace(&StartupWorkspace::Main, Some("Work")),
            None
        );
    }

    #[test]
    fn name_taken_is_case_insensitive_and_counts_mains_label() {
        let merged = merge_workspaces([&snapshot(
            vec![],
            vec![],
            vec![def("Work")],
            Some(def("Home")),
        )]);
        assert!(workspace_name_taken(&merged, "work", None));
        assert!(workspace_name_taken(&merged, " HOME ", None));
        assert!(!workspace_name_taken(&merged, "Play", None));
        // Renaming a workspace to a re-cased spelling of itself is allowed…
        assert!(!workspace_name_taken(&merged, "WORK", Some(&merged[1])));
        // …and so is relabelling Main to its own label.
        assert!(!workspace_name_taken(&merged, "home", Some(&merged[0])));
        assert!(workspace_name_taken(&merged, "work", Some(&merged[0])));
    }

    /// The merge is exact-name and never folds Main's label onto a definition,
    /// while a server refuses case-insensitive clashes. So when two servers
    /// disagree ("Work" on A, "work" on B; or B's Main labelled "Work"), the
    /// merged list itself is not a valid body for either — each server gets
    /// the wanted list narrowed to what it can accept, keeping its own
    /// spellings, so B's inconsistency never blocks an edit on A.
    #[test]
    fn definitions_for_a_server_drop_foreign_clashes_but_keep_its_own() {
        let wanted = vec![def("Work"), def("work"), def("Play"), def("Home")];
        // Server A defines "Work": B's "work" is dropped for A.
        let a_own = vec![def("Work")];
        assert_eq!(
            definitions_for_server(&wanted, &a_own, None),
            vec![def("Work"), def("Play"), def("Home")]
        );
        // Server B defines "work" and labels its Main "home": it keeps its own
        // spelling, and nothing clashing with its Main label is sent.
        let b_own = vec![def("work")];
        assert_eq!(
            definitions_for_server(&wanted, &b_own, Some("home")),
            vec![def("work"), def("Play")]
        );
        // A server that has neither keeps the first spelling.
        assert_eq!(
            definitions_for_server(&wanted, &[], None),
            vec![def("Work"), def("Play"), def("Home")]
        );
        // The result is always a body the server's own rule accepts.
        for (own, main) in [(&a_own, None), (&b_own, Some("home"))] {
            let req = claude_commander_protocol::workspace::SetWorkspacesRequest {
                workspaces: definitions_for_server(&wanted, own, main),
                main: None,
                startup_workspace: None,
            };
            assert!(
                claude_commander_protocol::workspace::validate_set_workspaces(&req).is_ok(),
                "{req:?}"
            );
        }
    }

    /// A server whose *own* stored list is already inconsistent is not
    /// silently repaired by dropping one of its definitions (that would orphan
    /// its projects' tags): its entries go through untouched and the server
    /// refuses, which the frontend reports.
    #[test]
    fn a_servers_own_definitions_are_never_dropped() {
        let wanted = vec![def("Home"), def("Play")];
        let own = vec![def("Home")];
        assert_eq!(
            definitions_for_server(&wanted, &own, Some("home")),
            vec![def("Home"), def("Play")]
        );
    }

    #[test]
    fn find_workspace_matches_names_then_mains_label() {
        let merged = merge_workspaces([&snapshot(
            vec![],
            vec![],
            vec![def("Work")],
            Some(def("Home")),
        )]);
        assert_eq!(
            find_workspace(&merged, "Work").unwrap().name.as_deref(),
            Some("Work")
        );
        assert_eq!(
            find_workspace(&merged, "work").unwrap().name.as_deref(),
            Some("Work")
        );
        assert!(find_workspace(&merged, "home").unwrap().is_main());
        assert!(find_workspace(&merged, "main").unwrap().is_main());
        assert!(find_workspace(&merged, "play").is_none());
    }
}

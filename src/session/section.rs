//! Section assignment for worktree sessions.
//!
//! Sessions are grouped under configurable section headers in the TUI list.
//! Assignment is a pure function of the session's PR-derived state and the
//! user's section configuration.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::git::PrState;
use crate::session::{SessionId, WorktreeSession};

/// Declarative predicate matching a session to a section.
/// All declared fields must match (AND); undeclared fields are ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SectionConfig {
    pub name: String,
    #[serde(default)]
    pub pr_state: Option<PrState>,
    #[serde(default)]
    pub is_draft: Option<bool>,
    #[serde(default)]
    pub has_label: Option<LabelPredicate>,
    #[serde(default)]
    pub has_pr: Option<bool>,
}

/// Label predicate: accepts either a single label (string in TOML) or a list
/// (array of strings, any-of semantics).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LabelPredicate {
    One(String),
    Any(Vec<String>),
}

impl LabelPredicate {
    fn matches(&self, labels: &[String]) -> bool {
        match self {
            Self::One(needle) => labels.iter().any(|l| l == needle),
            Self::Any(needles) => needles.iter().any(|n| labels.iter().any(|l| l == n)),
        }
    }
}

/// Result of assigning a session to a section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionAssignment {
    /// Matched a user-defined section by name.
    Matched(String),
    /// Did not match any section; falls into the catch-all.
    Other,
}

/// Compute the section a session belongs to given the configured sections.
///
/// Returns [`SectionAssignment::Other`] when no section's predicate matches.
pub fn assign_section(session: &WorktreeSession, sections: &[SectionConfig]) -> SectionAssignment {
    if let Some(name) = &session.section_override
        && sections.iter().any(|s| &s.name == name)
    {
        return SectionAssignment::Matched(name.clone());
    }
    for section in sections {
        if section_matches(session, section) {
            return SectionAssignment::Matched(section.name.clone());
        }
    }
    SectionAssignment::Other
}

/// Output group for one section in the rendered session list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSection {
    /// Section name (configured name, or the reserved literal `"Other"`).
    pub name: String,
    /// Session IDs in display order (oldest `entered_section_at` first).
    pub sessions: Vec<SessionId>,
}

/// Build the grouped, sorted section list for rendering.
///
/// Sessions are placed into the first matching section (or the built-in
/// "Other" bucket if no predicate matches). Within each group they are
/// sorted by `entered_section_at` ascending (oldest first).
pub fn build_sections(
    sessions: &[WorktreeSession],
    sections: &[SectionConfig],
) -> Vec<RenderedSection> {
    let other_idx = sections.len();
    let mut buckets: Vec<Vec<(SessionId, DateTime<Utc>)>> =
        (0..=other_idx).map(|_| Vec::new()).collect();

    for session in sessions {
        let idx = match assign_section(session, sections) {
            SectionAssignment::Matched(name) => sections
                .iter()
                .position(|s| s.name == name)
                .unwrap_or(other_idx),
            SectionAssignment::Other => other_idx,
        };
        buckets[idx].push((session.id, session.entered_section_at));
    }

    buckets
        .into_iter()
        .enumerate()
        .map(|(i, mut bucket)| {
            bucket.sort_by_key(|(_, ts)| *ts);
            let name = if i == other_idx {
                "Other".to_string()
            } else {
                sections[i].name.clone()
            };
            RenderedSection {
                name,
                sessions: bucket.into_iter().map(|(id, _)| id).collect(),
            }
        })
        .collect()
}

/// Recompute the session's section assignment and update
/// `current_section` + `entered_section_at` iff the section changed.
/// Returns `true` when a transition occurred.
pub fn apply_assignment(
    session: &mut WorktreeSession,
    sections: &[SectionConfig],
    now: DateTime<Utc>,
) -> bool {
    let new_name: Option<String> = match assign_section(session, sections) {
        SectionAssignment::Matched(name) => Some(name),
        SectionAssignment::Other => None,
    };
    if session.current_section == new_name {
        return false;
    }
    session.current_section = new_name;
    session.entered_section_at = now;
    true
}

fn section_matches(session: &WorktreeSession, section: &SectionConfig) -> bool {
    if let Some(required) = section.pr_state
        && session.pr_state != Some(required)
    {
        return false;
    }
    if let Some(required) = section.is_draft
        && session.pr_draft != required
    {
        return false;
    }
    if let Some(label_pred) = &section.has_label
        && !label_pred.matches(&session.pr_labels)
    {
        return false;
    }
    if let Some(required) = section.has_pr
        && session.pr_number.is_some() != required
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ProjectId, WorktreeSession};
    use chrono::{Duration, Utc};
    use std::path::PathBuf;

    fn make_session() -> WorktreeSession {
        WorktreeSession::new(
            ProjectId::new(),
            "test",
            "feature-branch",
            PathBuf::from("/tmp/unused"),
            "claude",
        )
    }

    #[test]
    fn empty_sections_config_yields_other() {
        let session = make_session();
        let sections: Vec<SectionConfig> = vec![];

        let result = assign_section(&session, &sections);

        assert_eq!(result, SectionAssignment::Other);
    }

    #[test]
    fn mismatched_pr_state_falls_through_to_other() {
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);

        let sections = vec![SectionConfig {
            name: "Merged".into(),
            pr_state: Some(PrState::Merged),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Other
        );
    }

    #[test]
    fn is_draft_predicate_matches_draft_session() {
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.pr_draft = true;

        let sections = vec![SectionConfig {
            name: "Drafts".into(),
            is_draft: Some(true),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("Drafts".into())
        );
    }

    #[test]
    fn and_semantics_require_all_fields_to_match() {
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.pr_draft = false;

        let sections = vec![SectionConfig {
            name: "Open drafts".into(),
            pr_state: Some(PrState::Open),
            is_draft: Some(true),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Other
        );
    }

    #[test]
    fn has_label_string_matches_when_session_has_label() {
        let mut session = make_session();
        session.pr_labels = vec!["ready-for-review".into(), "backend".into()];

        let sections = vec![SectionConfig {
            name: "Needs review".into(),
            has_label: Some(LabelPredicate::One("ready-for-review".into())),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("Needs review".into())
        );
    }

    #[test]
    fn has_label_string_falls_through_when_absent() {
        let mut session = make_session();
        session.pr_labels = vec!["backend".into()];

        let sections = vec![SectionConfig {
            name: "Needs review".into(),
            has_label: Some(LabelPredicate::One("ready-for-review".into())),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Other
        );
    }

    #[test]
    fn has_label_array_matches_any_of_the_labels() {
        let mut session = make_session();
        session.pr_labels = vec!["waiting-on-author".into()];

        let sections = vec![SectionConfig {
            name: "Blocked".into(),
            has_label: Some(LabelPredicate::Any(vec![
                "blocked".into(),
                "waiting-on-author".into(),
            ])),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("Blocked".into())
        );
    }

    #[test]
    fn has_pr_true_matches_session_with_pr_number() {
        let mut session = make_session();
        session.pr_number = Some(42);

        let sections = vec![SectionConfig {
            name: "Has PR".into(),
            has_pr: Some(true),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("Has PR".into())
        );
    }

    #[test]
    fn has_pr_false_matches_session_without_pr_number() {
        let session = make_session(); // pr_number None by default

        let sections = vec![SectionConfig {
            name: "No PR".into(),
            has_pr: Some(false),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("No PR".into())
        );
    }

    #[test]
    fn first_matching_section_wins_over_later_one() {
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.pr_labels = vec!["ready-for-review".into()];

        let sections = vec![
            SectionConfig {
                name: "Needs review".into(),
                has_label: Some(LabelPredicate::One("ready-for-review".into())),
                ..Default::default()
            },
            SectionConfig {
                name: "Open".into(),
                pr_state: Some(PrState::Open),
                ..Default::default()
            },
        ];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("Needs review".into())
        );
    }

    #[test]
    fn override_takes_precedence_over_predicate() {
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.section_override = Some("In progress".into());

        let sections = vec![
            SectionConfig {
                name: "Open".into(),
                pr_state: Some(PrState::Open),
                ..Default::default()
            },
            SectionConfig {
                name: "In progress".into(),
                ..Default::default()
            },
        ];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("In progress".into())
        );
    }

    #[test]
    fn stale_override_falls_back_to_predicate() {
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.section_override = Some("Deleted section".into());

        let sections = vec![SectionConfig {
            name: "Open".into(),
            pr_state: Some(PrState::Open),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("Open".into())
        );
    }

    #[test]
    fn apply_assignment_updates_timestamp_when_section_changes() {
        let mut session = make_session();
        let original = session.entered_section_at;
        let now = original + Duration::minutes(5);
        session.pr_state = Some(PrState::Open);

        let sections = vec![SectionConfig {
            name: "Open".into(),
            pr_state: Some(PrState::Open),
            ..Default::default()
        }];

        let changed = apply_assignment(&mut session, &sections, now);

        assert!(changed);
        assert_eq!(session.current_section.as_deref(), Some("Open"));
        assert_eq!(session.entered_section_at, now);
    }

    #[test]
    fn apply_assignment_noop_when_section_unchanged() {
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.current_section = Some("Open".into());
        let original = session.entered_section_at;

        let sections = vec![SectionConfig {
            name: "Open".into(),
            pr_state: Some(PrState::Open),
            ..Default::default()
        }];

        let changed = apply_assignment(&mut session, &sections, Utc::now() + Duration::hours(1));

        assert!(!changed);
        assert_eq!(session.entered_section_at, original);
    }

    #[test]
    fn sessions_sort_by_entered_section_at_ascending_within_section() {
        let earlier = Utc::now() - Duration::hours(2);
        let later = Utc::now() - Duration::hours(1);

        let mut older = make_session();
        older.pr_state = Some(PrState::Open);
        older.entered_section_at = earlier;

        let mut newer = make_session();
        newer.pr_state = Some(PrState::Open);
        newer.entered_section_at = later;

        // Intentionally reversed order in the input slice.
        let sessions = vec![newer.clone(), older.clone()];
        let sections = vec![SectionConfig {
            name: "Open".into(),
            pr_state: Some(PrState::Open),
            ..Default::default()
        }];

        let groups = build_sections(&sessions, &sections);
        let open = groups
            .iter()
            .find(|g| g.name == "Open")
            .expect("Open section present");

        assert_eq!(open.sessions, vec![older.id, newer.id]);
    }

    #[test]
    fn empty_sections_config_collects_all_sessions_into_other() {
        let s1 = make_session();
        let s2 = make_session();

        let groups = build_sections(&[s1.clone(), s2.clone()], &[]);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "Other");
        assert_eq!(groups[0].sessions.len(), 2);
    }

    #[test]
    fn empty_sections_still_rendered_with_zero_sessions() {
        let sections = vec![
            SectionConfig {
                name: "Drafts".into(),
                is_draft: Some(true),
                ..Default::default()
            },
            SectionConfig {
                name: "Open".into(),
                pr_state: Some(PrState::Open),
                ..Default::default()
            },
        ];

        let groups = build_sections(&[], &sections);

        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].name, "Drafts");
        assert!(groups[0].sessions.is_empty());
        assert_eq!(groups[1].name, "Open");
        assert!(groups[1].sessions.is_empty());
    }

    #[test]
    fn other_section_is_last() {
        let sections = vec![SectionConfig {
            name: "Open".into(),
            pr_state: Some(PrState::Open),
            ..Default::default()
        }];

        let groups = build_sections(&[], &sections);

        assert_eq!(groups.last().unwrap().name, "Other");
    }

    #[test]
    fn setting_override_then_applying_moves_session_and_updates_timestamp() {
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.current_section = Some("Open".into());
        let original = session.entered_section_at;
        let now = original + Duration::hours(1);

        let sections = vec![
            SectionConfig {
                name: "Open".into(),
                pr_state: Some(PrState::Open),
                ..Default::default()
            },
            SectionConfig {
                name: "In progress".into(),
                ..Default::default()
            },
        ];

        // User pins to "In progress".
        session.section_override = Some("In progress".into());
        let changed = apply_assignment(&mut session, &sections, now);

        assert!(changed);
        assert_eq!(session.current_section.as_deref(), Some("In progress"));
        assert_eq!(session.entered_section_at, now);
    }

    #[test]
    fn clearing_override_returns_session_to_auto_rules() {
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.section_override = Some("In progress".into());
        session.current_section = Some("In progress".into());
        let later = session.entered_section_at + Duration::hours(1);

        let sections = vec![
            SectionConfig {
                name: "Open".into(),
                pr_state: Some(PrState::Open),
                ..Default::default()
            },
            SectionConfig {
                name: "In progress".into(),
                ..Default::default()
            },
        ];

        // Clear the override.
        session.section_override = None;
        let changed = apply_assignment(&mut session, &sections, later);

        assert!(changed);
        assert_eq!(session.current_section.as_deref(), Some("Open"));
        assert_eq!(session.entered_section_at, later);
    }

    #[test]
    fn pr_state_predicate_matches_open_session() {
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);

        let sections = vec![SectionConfig {
            name: "Open PRs".into(),
            pr_state: Some(PrState::Open),
            ..Default::default()
        }];

        let result = assign_section(&session, &sections);

        assert_eq!(result, SectionAssignment::Matched("Open PRs".into()));
    }
}

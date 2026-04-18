//! Section assignment for worktree sessions.
//!
//! Sessions are grouped under configurable section headers in the TUI list.
//! Assignment is a pure function of the session's PR-derived state and the
//! user's section configuration.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::git::{PrState, ReviewDecision};
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
    #[serde(default)]
    pub review_decision: Option<DecisionPredicate>,
}

/// Review-decision predicate: accepts a single value (string in TOML) or a
/// list (array of strings, any-of semantics).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DecisionPredicate {
    One(ReviewDecision),
    Any(Vec<ReviewDecision>),
}

impl DecisionPredicate {
    fn matches(&self, decision: Option<ReviewDecision>) -> bool {
        let Some(d) = decision else {
            return false;
        };
        match self {
            Self::One(needle) => *needle == d,
            Self::Any(needles) => needles.contains(&d),
        }
    }
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
    /// Did not match any section; falls into the implicit "In Progress"
    /// catch-all (process position 0).
    InProgress,
}

/// Compute the section a session belongs to given the configured sections.
///
/// Returns [`SectionAssignment::InProgress`] when no section's predicate
/// matches.
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
    SectionAssignment::InProgress
}

/// Reserved name of the implicit catch-all section, always at process
/// position 0 (displayed first).
pub const IN_PROGRESS: &str = "In Progress";

/// Output group for one section in the rendered session list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSection {
    /// Section name (configured name, or the reserved [`IN_PROGRESS`] literal).
    pub name: String,
    /// Session IDs in display order (oldest `entered_section_at` first).
    pub sessions: Vec<SessionId>,
}

/// Build the grouped, sorted section list for rendering.
///
/// "In Progress" (the implicit catch-all) is always returned first, followed
/// by the user-configured sections in declared order. Sessions are placed by
/// their cached `current_section` (the source of truth maintained by
/// [`apply_assignment`]). A `current_section` referring to a section no
/// longer in config falls back to "In Progress". Within each group they are
/// sorted by `entered_section_at` ascending (oldest first).
pub fn build_sections(
    sessions: &[WorktreeSession],
    sections: &[SectionConfig],
) -> Vec<RenderedSection> {
    // Bucket 0 = In Progress; buckets 1..=N = user sections.
    let mut buckets: Vec<Vec<(SessionId, DateTime<Utc>)>> =
        (0..=sections.len()).map(|_| Vec::new()).collect();

    for session in sessions {
        let idx = section_position(session.current_section.as_deref(), sections);
        buckets[idx].push((session.id, session.entered_section_at));
    }

    buckets
        .into_iter()
        .enumerate()
        .map(|(i, mut bucket)| {
            bucket.sort_by_key(|(_, ts)| *ts);
            let name = if i == 0 {
                IN_PROGRESS.to_string()
            } else {
                sections[i - 1].name.clone()
            };
            RenderedSection {
                name,
                sessions: bucket.into_iter().map(|(id, _)| id).collect(),
            }
        })
        .collect()
}

/// Process-order position of a section name. 0 = In Progress (catch-all),
/// 1.. = user sections in declared order. Unknown names → 0 (treated as
/// In Progress, e.g. stale `current_section` referring to a deleted section).
fn section_position(name: Option<&str>, sections: &[SectionConfig]) -> usize {
    match name {
        None => 0,
        Some(n) => sections
            .iter()
            .position(|s| s.name == n)
            .map(|i| i + 1)
            .unwrap_or(0),
    }
}

/// Recompute the session's section assignment and update
/// `current_section` + `entered_section_at` iff the section changed.
///
/// `section_override` is a hard lock: when set to a configured section, the
/// session is pinned there regardless of process order or predicates, and
/// auto cannot advance it. Stale overrides (referring to a section no
/// longer in config) are ignored.
///
/// Otherwise, auto moves are **forward-only** in process order: a session
/// that would match a section at a lower process position than its current
/// one stays put.
///
/// Returns `true` when a transition occurred.
pub fn apply_assignment(
    session: &mut WorktreeSession,
    sections: &[SectionConfig],
    now: DateTime<Utc>,
) -> bool {
    // Override path: bypass forward-only and predicates entirely.
    if let Some(name) = &session.section_override
        && sections.iter().any(|s| &s.name == name)
    {
        let target = Some(name.clone());
        if session.current_section == target {
            return false;
        }
        session.current_section = target;
        session.entered_section_at = now;
        return true;
    }

    let new_name: Option<String> = match assign_section(session, sections) {
        SectionAssignment::Matched(name) => Some(name),
        SectionAssignment::InProgress => None,
    };

    let cur_pos = section_position(session.current_section.as_deref(), sections);
    let new_pos = section_position(new_name.as_deref(), sections);
    if new_pos < cur_pos {
        return false;
    }

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
    if let Some(decision_pred) = &section.review_decision
        && !decision_pred.matches(session.review_decision)
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
    fn review_decision_array_matches_any_of() {
        let mut session = make_session();
        session.review_decision = Some(ReviewDecision::ChangesRequested);

        let sections = vec![SectionConfig {
            name: "In Review".into(),
            review_decision: Some(DecisionPredicate::Any(vec![
                ReviewDecision::ChangesRequested,
                ReviewDecision::ReviewRequired,
            ])),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("In Review".into())
        );
    }

    #[test]
    fn review_decision_predicate_falls_through_when_session_has_no_decision() {
        let session = make_session(); // review_decision = None

        let sections = vec![SectionConfig {
            name: "Approved".into(),
            review_decision: Some(DecisionPredicate::One(ReviewDecision::Approved)),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::InProgress
        );
    }

    #[test]
    fn review_decision_approved_predicate_matches() {
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.review_decision = Some(ReviewDecision::Approved);

        let sections = vec![SectionConfig {
            name: "Ready to Merge".into(),
            review_decision: Some(DecisionPredicate::One(ReviewDecision::Approved)),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("Ready to Merge".into())
        );
    }

    #[test]
    fn empty_sections_config_yields_other() {
        let session = make_session();
        let sections: Vec<SectionConfig> = vec![];

        let result = assign_section(&session, &sections);

        assert_eq!(result, SectionAssignment::InProgress);
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
            SectionAssignment::InProgress
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
            SectionAssignment::InProgress
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
            SectionAssignment::InProgress
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
    fn apply_assignment_rebases_session_with_stale_current_section() {
        // current_section refers to a section that's no longer in config —
        // it's treated as position 0 (In Progress), so any matching
        // predicate is a valid forward move.
        let sections = vec![SectionConfig {
            name: "Open".into(),
            pr_state: Some(PrState::Open),
            ..Default::default()
        }];
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.current_section = Some("Removed Section".into());

        let now = session.entered_section_at + Duration::minutes(1);
        let changed = apply_assignment(&mut session, &sections, now);

        assert!(changed);
        assert_eq!(session.current_section.as_deref(), Some("Open"));
    }

    #[test]
    fn override_bypasses_forward_only_rule() {
        // Process order: 0=In Progress, 1=Open, 2=Pinned
        let sections = vec![
            SectionConfig {
                name: "Open".into(),
                pr_state: Some(PrState::Open),
                ..Default::default()
            },
            SectionConfig {
                name: "Pinned".into(),
                ..Default::default()
            },
        ];
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.current_section = Some("Pinned".into());
        let original = session.entered_section_at;
        let later = original + Duration::hours(1);

        // User pins backward to "Open" (backward in process order).
        session.section_override = Some("Open".into());
        let changed = apply_assignment(&mut session, &sections, later);

        assert!(changed);
        assert_eq!(session.current_section.as_deref(), Some("Open"));
        assert_eq!(session.entered_section_at, later);
    }

    #[test]
    fn override_locks_section_against_auto_advancement() {
        let sections = vec![
            SectionConfig {
                name: "Needs Review".into(),
                has_label: Some(LabelPredicate::One("dev-review-required".into())),
                ..Default::default()
            },
            SectionConfig {
                name: "In Review".into(),
                review_decision: Some(DecisionPredicate::One(ReviewDecision::ChangesRequested)),
                ..Default::default()
            },
        ];
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.section_override = Some("Needs Review".into());
        session.current_section = Some("Needs Review".into());
        let original = session.entered_section_at;

        // Reviewer requests changes — predicate would advance the session
        // to "In Review", but the override locks it.
        session.review_decision = Some(ReviewDecision::ChangesRequested);

        let later = original + Duration::hours(1);
        let changed = apply_assignment(&mut session, &sections, later);

        assert!(!changed, "auto must not advance past an override");
        assert_eq!(session.current_section.as_deref(), Some("Needs Review"));
        assert_eq!(session.entered_section_at, original);
    }

    #[test]
    fn apply_assignment_refuses_backward_auto_move() {
        // Process order: 0=In Progress, 1=Needs Review, 2=In Review
        let sections = vec![
            SectionConfig {
                name: "Needs Review".into(),
                has_label: Some(LabelPredicate::One("dev-review-required".into())),
                ..Default::default()
            },
            SectionConfig {
                name: "In Review".into(),
                review_decision: Some(DecisionPredicate::One(ReviewDecision::ChangesRequested)),
                ..Default::default()
            },
        ];

        // Session is currently in "Needs Review" (position 1).
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.pr_labels = vec!["dev-review-required".into()];
        session.current_section = Some("Needs Review".into());
        let original_stamp = session.entered_section_at;

        // Reviewer removes the label without doing anything else.
        // Predicate would now place the session in In Progress (position 0).
        session.pr_labels.clear();

        let later = original_stamp + Duration::hours(1);
        let changed = apply_assignment(&mut session, &sections, later);

        assert!(!changed, "auto move backward should be refused");
        assert_eq!(
            session.current_section.as_deref(),
            Some("Needs Review"),
            "session must stay where it was"
        );
        assert_eq!(session.entered_section_at, original_stamp);
    }

    #[test]
    fn apply_assignment_allows_forward_auto_move() {
        let sections = vec![
            SectionConfig {
                name: "Needs Review".into(),
                has_label: Some(LabelPredicate::One("dev-review-required".into())),
                ..Default::default()
            },
            SectionConfig {
                name: "In Review".into(),
                review_decision: Some(DecisionPredicate::One(ReviewDecision::ChangesRequested)),
                ..Default::default()
            },
        ];

        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.pr_labels = vec!["dev-review-required".into()];
        session.current_section = Some("Needs Review".into());
        let original_stamp = session.entered_section_at;

        // Reviewer requests changes (label may or may not be present; here, removed).
        session.pr_labels.clear();
        session.review_decision = Some(ReviewDecision::ChangesRequested);

        let later = original_stamp + Duration::hours(1);
        let changed = apply_assignment(&mut session, &sections, later);

        assert!(changed);
        assert_eq!(session.current_section.as_deref(), Some("In Review"));
        assert_eq!(session.entered_section_at, later);
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
        older.current_section = Some("Open".into());
        older.entered_section_at = earlier;

        let mut newer = make_session();
        newer.pr_state = Some(PrState::Open);
        newer.current_section = Some("Open".into());
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
    fn empty_sections_config_collects_all_sessions_into_in_progress() {
        let s1 = make_session();
        let s2 = make_session();

        let groups = build_sections(&[s1.clone(), s2.clone()], &[]);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, IN_PROGRESS);
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
        assert_eq!(groups[0].name, IN_PROGRESS);
        assert!(groups[0].sessions.is_empty());
        assert_eq!(groups[1].name, "Drafts");
        assert!(groups[1].sessions.is_empty());
        assert_eq!(groups[2].name, "Open");
        assert!(groups[2].sessions.is_empty());
    }

    #[test]
    fn build_sections_honours_current_section_over_live_predicate() {
        // Session was forward-moved into "Needs Review", then the label was
        // removed. apply_assignment refused the backward move so
        // current_section is still "Needs Review". build_sections must place
        // the session there too, not re-evaluate predicates.
        let sections = vec![SectionConfig {
            name: "Needs Review".into(),
            has_label: Some(LabelPredicate::One("dev-review-required".into())),
            ..Default::default()
        }];
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.pr_labels.clear(); // label removed
        session.current_section = Some("Needs Review".into());

        let groups = build_sections(&[session.clone()], &sections);
        let needs_review = groups
            .iter()
            .find(|g| g.name == "Needs Review")
            .expect("Needs Review present");
        assert_eq!(needs_review.sessions, vec![session.id]);
    }

    #[test]
    fn in_progress_catchall_is_first() {
        let sections = vec![SectionConfig {
            name: "Open".into(),
            pr_state: Some(PrState::Open),
            ..Default::default()
        }];

        let groups = build_sections(&[], &sections);

        assert_eq!(groups.first().unwrap().name, "In Progress");
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
    fn clearing_override_does_not_pull_session_backward_in_process_order() {
        // Process order: 0=In Progress, 1=Open, 2=Pinned
        let sections = vec![
            SectionConfig {
                name: "Open".into(),
                pr_state: Some(PrState::Open),
                ..Default::default()
            },
            SectionConfig {
                name: "Pinned".into(),
                ..Default::default()
            },
        ];
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.section_override = Some("Pinned".into());
        session.current_section = Some("Pinned".into());
        let original = session.entered_section_at;
        let later = original + Duration::hours(1);

        // Clear the override; predicate would put session at "Open" (pos 1),
        // which is backward from "Pinned" (pos 2). Forward-only rule keeps
        // it where it is.
        session.section_override = None;
        let changed = apply_assignment(&mut session, &sections, later);

        assert!(!changed);
        assert_eq!(session.current_section.as_deref(), Some("Pinned"));
        assert_eq!(session.entered_section_at, original);
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

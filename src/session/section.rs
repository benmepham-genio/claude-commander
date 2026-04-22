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
    pub pr_state: Option<StatePredicate>,
    #[serde(default)]
    pub is_draft: Option<bool>,
    #[serde(default)]
    pub has_label: Option<LabelPredicate>,
    #[serde(default)]
    pub has_pr: Option<bool>,
    #[serde(default)]
    pub review_decision: Option<DecisionPredicate>,
    #[serde(default)]
    pub has_reviewer: Option<ReviewerPredicate>,
}

/// Reviewer predicate.
///
/// Accepts:
/// - `true` — at least one reviewer that isn't Copilot (its GitHub bot
///   login is matched case-insensitively, so `copilot-pull-request-reviewer[bot]`
///   or any variant is excluded).
/// - `false` — no non-Copilot reviewers on the PR.
/// - a specific login string — matches literally (no Copilot filtering).
/// - an array of login strings — any-of, literal match.
///
/// "Reviewer" = the union of requested reviewers and submitted review authors.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReviewerPredicate {
    Bool(bool),
    One(String),
    Any(Vec<String>),
}

impl ReviewerPredicate {
    fn matches(&self, reviewers: &[String]) -> bool {
        match self {
            Self::Bool(true) => reviewers.iter().any(|r| !is_copilot_login(r)),
            Self::Bool(false) => !reviewers.iter().any(|r| !is_copilot_login(r)),
            Self::One(needle) => reviewers.iter().any(|r| r == needle),
            Self::Any(needles) => needles.iter().any(|n| reviewers.iter().any(|r| r == n)),
        }
    }
}

fn is_copilot_login(login: &str) -> bool {
    login.to_lowercase().contains("copilot")
}

/// Accepts either a single value (scalar in TOML) or a list (array, any-of
/// semantics). Used for predicate fields where the session's value is a
/// single `Copy` enum, like `pr_state` and `review_decision`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Any(Vec<T>),
}

impl<T: PartialEq + Copy> OneOrMany<T> {
    fn matches(&self, value: Option<T>) -> bool {
        let Some(v) = value else { return false };
        match self {
            Self::One(needle) => *needle == v,
            Self::Any(needles) => needles.contains(&v),
        }
    }
}

pub type StatePredicate = OneOrMany<PrState>;
pub type DecisionPredicate = OneOrMany<ReviewDecision>;

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

/// Where a session should be placed, given its current position and the
/// configured sections.
///
/// Evaluation is forward-only by construction: the predicate scan starts at
/// the session's current config index and never considers earlier sections.
/// A session therefore stays in its current section until a *later* predicate
/// matches, or is moved manually via [`WorktreeSession::section_override`].
///
/// Rules in order:
/// 1. If `section_override` names a configured section, use it (hard lock).
/// 2. Otherwise scan `sections[start..]` and return the first predicate match,
///    where `start` is the current section's config index (or 0 if the
///    session has no current section, its current section is predicate-less,
///    or its current section no longer exists).
/// 3. If nothing matches in that range, stay where we were. If `current_section`
///    doesn't exist in the config, fall to [`SectionAssignment::InProgress`].
pub fn assign_section(session: &WorktreeSession, sections: &[SectionConfig]) -> SectionAssignment {
    if let Some(name) = &session.section_override
        && sections.iter().any(|s| &s.name == name)
    {
        return SectionAssignment::Matched(name.clone());
    }

    let start = session
        .current_section
        .as_deref()
        .and_then(|n| sections.iter().position(|s| s.name == n))
        .filter(|&i| has_predicates(&sections[i]))
        .unwrap_or(0);

    for section in &sections[start..] {
        if has_predicates(section) && section_matches(session, section) {
            return SectionAssignment::Matched(section.name.clone());
        }
    }

    match &session.current_section {
        Some(name) if sections.iter().any(|s| &s.name == name) => {
            SectionAssignment::Matched(name.clone())
        }
        _ => SectionAssignment::InProgress,
    }
}

/// True when a section declares at least one predicate field; otherwise the
/// section is a manual-only waypoint (reachable only via override).
fn has_predicates(section: &SectionConfig) -> bool {
    section.pr_state.is_some()
        || section.is_draft.is_some()
        || section.has_label.is_some()
        || section.has_pr.is_some()
        || section.review_decision.is_some()
        || section.has_reviewer.is_some()
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
        let idx = display_index(session.current_section.as_deref(), sections);
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

/// Display bucket index, used by [`build_sections`] for rendering. 0 = the
/// implicit "In Progress" row at the top. 1..=N = each user-declared section
/// in config order (predicate-bearing *and* manual-only). Unknown names fall
/// to 0.
fn display_index(name: Option<&str>, sections: &[SectionConfig]) -> usize {
    let Some(n) = name else { return 0 };
    sections
        .iter()
        .position(|s| s.name == n)
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// Recompute the session's section assignment and update
/// `current_section` + `entered_section_at` iff the section changed.
/// Returns `true` when a transition occurred.
///
/// Forward-only semantics live inside [`assign_section`] itself — the scan
/// only looks at sections at or after the session's current config index.
pub fn apply_assignment(
    session: &mut WorktreeSession,
    sections: &[SectionConfig],
    now: DateTime<Utc>,
) -> bool {
    let new_name: Option<String> = match assign_section(session, sections) {
        SectionAssignment::Matched(name) => Some(name),
        SectionAssignment::InProgress => None,
    };
    if session.current_section == new_name {
        return false;
    }
    session.current_section = new_name;
    session.entered_section_at = now;
    true
}

fn section_matches(session: &WorktreeSession, section: &SectionConfig) -> bool {
    if let Some(state_pred) = &section.pr_state
        && !state_pred.matches(session.pr_state)
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
    if let Some(reviewer_pred) = &section.has_reviewer
        && !reviewer_pred.matches(&session.pr_reviewers)
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
            pr_state: Some(StatePredicate::One(PrState::Merged)),
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
            pr_state: Some(StatePredicate::One(PrState::Open)),
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
            pr_state: Some(StatePredicate::One(PrState::Open)),
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
    fn session_pinned_to_manual_only_section_renders_in_that_bucket() {
        // A session manually moved to "Stale" must render under "Stale",
        // not fall into In Progress. Display order follows config order
        // regardless of whether a section has predicates.
        let sections = vec![
            SectionConfig {
                name: "Needs Review".into(),
                has_label: Some(LabelPredicate::One("dev-review-required".into())),
                ..Default::default()
            },
            SectionConfig {
                name: "Stale".into(),
                ..Default::default()
            },
        ];
        let mut session = make_session();
        session.current_section = Some("Stale".into());

        let groups = build_sections(&[session.clone()], &sections);

        assert_eq!(
            groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
            vec!["In Progress", "Needs Review", "Stale"]
        );
        let stale = groups.iter().find(|g| g.name == "Stale").unwrap();
        assert_eq!(stale.sessions, vec![session.id]);
        let in_progress = groups.iter().find(|g| g.name == "In Progress").unwrap();
        assert!(in_progress.sessions.is_empty());
    }

    #[test]
    fn assign_section_ignores_earlier_predicates_from_current_index() {
        // Sections in priority order: Open (index 0), In Review (index 1).
        // A session already in "In Review" must not slide back to "Open" just
        // because "Open" still matches — auto-evaluation is forward-only by
        // construction: the scan starts at the session's current config index
        // and never looks at earlier sections.
        let sections = vec![
            SectionConfig {
                name: "Open".into(),
                pr_state: Some(StatePredicate::One(PrState::Open)),
                ..Default::default()
            },
            SectionConfig {
                name: "In Review".into(),
                review_decision: Some(DecisionPredicate::One(ReviewDecision::ChangesRequested)),
                ..Default::default()
            },
        ];
        let mut session = make_session();
        session.pr_state = Some(PrState::Open); // would match Open at index 0
        session.review_decision = None; // In Review's predicate no longer matches
        session.current_section = Some("In Review".into());

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("In Review".into())
        );
    }

    #[test]
    fn predicate_less_section_is_never_auto_matched() {
        // "Stale" has no predicates — declaring it first would, under the
        // old matching rule, swallow every session. It should instead be a
        // manual-only waypoint.
        let sections = vec![
            SectionConfig {
                name: "Stale".into(),
                ..Default::default()
            },
            SectionConfig {
                name: "Needs Review".into(),
                has_label: Some(LabelPredicate::One("dev-review-required".into())),
                ..Default::default()
            },
        ];
        let session = make_session(); // no PR data

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::InProgress
        );
    }

    #[test]
    fn override_still_reaches_predicate_less_section() {
        let sections = vec![SectionConfig {
            name: "Stale".into(),
            ..Default::default()
        }];
        let mut session = make_session();
        session.section_override = Some("Stale".into());

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("Stale".into())
        );
    }

    #[test]
    fn clearing_override_from_predicate_less_section_does_not_get_blocked() {
        // Session is pinned to "Stale" (manual-only, declared last).
        // Clearing the override should let predicate-driven auto move the
        // session into a predicate section — forward-only must treat
        // manual-only sections as position 0, not their config index.
        let sections = vec![
            SectionConfig {
                name: "Needs Review".into(),
                has_label: Some(LabelPredicate::One("dev-review-required".into())),
                ..Default::default()
            },
            SectionConfig {
                name: "Stale".into(),
                ..Default::default()
            },
        ];
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);
        session.pr_labels = vec!["dev-review-required".into()];
        session.current_section = Some("Stale".into());
        let original = session.entered_section_at;
        let later = original + Duration::hours(1);

        session.section_override = None;
        let changed = apply_assignment(&mut session, &sections, later);

        assert!(changed);
        assert_eq!(session.current_section.as_deref(), Some("Needs Review"));
        assert_eq!(session.entered_section_at, later);
    }

    #[test]
    fn override_bypasses_forward_only_rule() {
        // Process order: 0=In Progress, 1=Open, 2=Pinned
        let sections = vec![
            SectionConfig {
                name: "Open".into(),
                pr_state: Some(StatePredicate::One(PrState::Open)),
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
                pr_state: Some(StatePredicate::One(PrState::Open)),
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
                pr_state: Some(StatePredicate::One(PrState::Open)),
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
            pr_state: Some(StatePredicate::One(PrState::Open)),
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
            pr_state: Some(StatePredicate::One(PrState::Open)),
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
            pr_state: Some(StatePredicate::One(PrState::Open)),
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
            pr_state: Some(StatePredicate::One(PrState::Open)),
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
                pr_state: Some(StatePredicate::One(PrState::Open)),
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
            pr_state: Some(StatePredicate::One(PrState::Open)),
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
                pr_state: Some(StatePredicate::One(PrState::Open)),
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
    fn pr_state_array_matches_any_of() {
        let mut merged_session = make_session();
        merged_session.pr_state = Some(PrState::Merged);
        let mut closed_session = make_session();
        closed_session.pr_state = Some(PrState::Closed);
        let mut open_session = make_session();
        open_session.pr_state = Some(PrState::Open);

        let sections = vec![SectionConfig {
            name: "Done".into(),
            pr_state: Some(StatePredicate::Any(vec![PrState::Merged, PrState::Closed])),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&merged_session, &sections),
            SectionAssignment::Matched("Done".into())
        );
        assert_eq!(
            assign_section(&closed_session, &sections),
            SectionAssignment::Matched("Done".into())
        );
        assert_eq!(
            assign_section(&open_session, &sections),
            SectionAssignment::InProgress
        );
    }

    #[test]
    fn has_reviewer_true_matches_session_with_human_reviewer() {
        let mut session = make_session();
        session.pr_reviewers = vec!["alice".into()];

        let sections = vec![SectionConfig {
            name: "In Review".into(),
            has_reviewer: Some(ReviewerPredicate::Bool(true)),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("In Review".into())
        );
    }

    #[test]
    fn has_reviewer_true_ignores_copilot_only_reviewers() {
        // Copilot's reviewer bot login. `has_reviewer = true` means
        // "engaged by someone other than Copilot" and must not match a PR
        // where Copilot is the only reviewer.
        let mut session = make_session();
        session.pr_reviewers = vec!["copilot-pull-request-reviewer[bot]".into()];

        let sections = vec![SectionConfig {
            name: "In Review".into(),
            has_reviewer: Some(ReviewerPredicate::Bool(true)),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::InProgress
        );
    }

    #[test]
    fn has_reviewer_specific_login_matches_literally() {
        let mut session = make_session();
        session.pr_reviewers = vec!["alice".into(), "bob".into()];

        let sections = vec![SectionConfig {
            name: "Alice's".into(),
            has_reviewer: Some(ReviewerPredicate::One("alice".into())),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("Alice's".into())
        );
    }

    #[test]
    fn has_reviewer_array_matches_any_of_the_logins() {
        let mut session = make_session();
        session.pr_reviewers = vec!["bob".into()];

        let sections = vec![SectionConfig {
            name: "Team".into(),
            has_reviewer: Some(ReviewerPredicate::Any(vec!["alice".into(), "bob".into()])),
            ..Default::default()
        }];

        assert_eq!(
            assign_section(&session, &sections),
            SectionAssignment::Matched("Team".into())
        );
    }

    #[test]
    fn pr_state_predicate_matches_open_session() {
        let mut session = make_session();
        session.pr_state = Some(PrState::Open);

        let sections = vec![SectionConfig {
            name: "Open PRs".into(),
            pr_state: Some(StatePredicate::One(PrState::Open)),
            ..Default::default()
        }];

        let result = assign_section(&session, &sections);

        assert_eq!(result, SectionAssignment::Matched("Open PRs".into()));
    }
}

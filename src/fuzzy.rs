//! Fuzzy (subsequence) matching used by the quick-select palette.

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use std::sync::OnceLock;

fn matcher() -> &'static SkimMatcherV2 {
    static M: OnceLock<SkimMatcherV2> = OnceLock::new();
    // `ignore_case` matches regardless of needle case — preserves the
    // original lowercase-then-contains behaviour users were used to.
    M.get_or_init(|| SkimMatcherV2::default().ignore_case())
}

/// Score `needle` against `haystack` using Skim-style fuzzy matching.
///
/// Returns `Some(score)` when every char of `needle` appears in `haystack`
/// in order (case-insensitive), `None` otherwise. Higher score = better
/// match. An empty needle always scores `Some(0)` so callers can filter
/// without special-casing the empty-query path.
pub fn fuzzy_score(haystack: &str, needle: &str) -> Option<i64> {
    if needle.is_empty() {
        return Some(0);
    }
    matcher().fuzzy_match(haystack, needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_matches() {
        assert!(fuzzy_score("android-record-2", "andr2").is_some());
        assert!(fuzzy_score("android-record-2", "rec2").is_some());
        assert!(fuzzy_score("android-record-2", "and-2").is_some());
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert!(fuzzy_score("android-record-2", "xyz").is_none());
        // Out-of-order chars: needle order must be preserved.
        assert!(fuzzy_score("android-record-2", "2andr").is_none());
    }

    #[test]
    fn case_insensitive() {
        assert!(fuzzy_score("Android-Record-2", "andr2").is_some());
        assert!(fuzzy_score("android-record-2", "ANDR2").is_some());
    }

    #[test]
    fn empty_needle_matches_everything() {
        assert_eq!(fuzzy_score("anything", ""), Some(0));
        assert_eq!(fuzzy_score("", ""), Some(0));
    }

    #[test]
    fn contiguous_outranks_scattered() {
        let tight = fuzzy_score("android", "andr").unwrap();
        let loose = fuzzy_score("a-n-d-r-oid", "andr").unwrap();
        assert!(tight > loose, "tight={tight} loose={loose}");
    }
}

use serde::Serialize;
use uuid::Uuid;

#[derive(Serialize)]
pub struct DiscoveryMatch {
    pub id: Uuid,
    pub name: String,
}

#[derive(Serialize)]
pub struct DiscoverySuggestion {
    #[serde(rename = "match")]
    pub matched: Option<DiscoveryMatch>,
    pub confidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_units: Option<String>,
}


/// Classify a source-supplied name against the entities it could name: `exact` on a
/// case-insensitive equality, `fuzzy` on containment either way, `none` otherwise. The first
/// candidate in order wins each tier.
#[must_use]
pub fn match_confidence(name: &str, candidates: &[(Uuid, String)]) -> DiscoverySuggestion {
    let lower = name.to_lowercase();
    // Exact case-insensitive match
    if let Some((id, cname)) = candidates.iter().find(|(_, n)| n.to_lowercase() == lower) {
        return DiscoverySuggestion {
            matched: Some(DiscoveryMatch {
                id: *id,
                name: cname.clone(),
            }),
            confidence: "exact".to_string(),
            suggested_name: None,
            suggested_units: None,
        };
    }
    // Fuzzy: substring containment
    if let Some((id, cname)) = candidates
        .iter()
        .find(|(_, n)| n.to_lowercase().contains(&lower) || lower.contains(&n.to_lowercase()))
    {
        return DiscoverySuggestion {
            matched: Some(DiscoveryMatch {
                id: *id,
                name: cname.clone(),
            }),
            confidence: "fuzzy".to_string(),
            suggested_name: None,
            suggested_units: None,
        };
    }
    DiscoverySuggestion {
        matched: None,
        confidence: "none".to_string(),
        suggested_name: Some(name.to_string()),
        suggested_units: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(names: &[&str]) -> Vec<(Uuid, String)> {
        names
            .iter()
            .map(|n| (Uuid::new_v4(), (*n).to_string()))
            .collect()
    }

    #[test]
    fn test_match_confidence_exact_ignores_case() {
        let c = candidates(&["Saxon", "Martigny"]);
        let s = match_confidence("saxon", &c);
        assert_eq!(s.confidence, "exact");
        assert_eq!(s.matched.unwrap().name, "Saxon");
        assert!(s.suggested_name.is_none());
    }

    #[test]
    fn test_match_confidence_exact_beats_a_containing_candidate() {
        let c = candidates(&["Saxon village", "Saxon"]);
        let s = match_confidence("Saxon", &c);
        assert_eq!(s.confidence, "exact");
        assert_eq!(s.matched.unwrap().name, "Saxon");
    }

    #[test]
    fn test_match_confidence_fuzzy_when_the_candidate_contains_the_name() {
        let c = candidates(&["Martigny", "Saxon"]);
        let s = match_confidence("Sax", &c);
        assert_eq!(s.confidence, "fuzzy");
        assert_eq!(s.matched.unwrap().name, "Saxon");
    }

    #[test]
    fn test_match_confidence_fuzzy_when_the_name_contains_the_candidate() {
        let c = candidates(&["Saxon"]);
        let s = match_confidence("Saxon downstream", &c);
        assert_eq!(s.confidence, "fuzzy");
        assert_eq!(s.matched.unwrap().name, "Saxon");
    }

    // Containment is bidirectional and unweighted, so a one-character name takes the first
    // candidate on the list. The wizard shows this as a suggestion an operator must confirm.
    #[test]
    fn test_match_confidence_single_character_name_matches_the_first_candidate() {
        let c = candidates(&["Martigny", "Saxon"]);
        let s = match_confidence("a", &c);
        assert_eq!(s.confidence, "fuzzy");
        assert_eq!(s.matched.unwrap().name, "Martigny");
    }

    #[test]
    fn test_match_confidence_none_suggests_the_source_name() {
        let s = match_confidence("Verbier", &candidates(&["Martigny", "Saxon"]));
        assert_eq!(s.confidence, "none");
        assert!(s.matched.is_none());
        assert_eq!(s.suggested_name.as_deref(), Some("Verbier"));
    }

    #[test]
    fn test_match_confidence_no_candidates_is_none() {
        let s = match_confidence("Saxon", &[]);
        assert_eq!(s.confidence, "none");
        assert!(s.matched.is_none());
    }

    #[test]
    fn test_match_confidence_empty_name_matches_everything() {
        let s = match_confidence("", &candidates(&["Saxon"]));
        assert_eq!(s.confidence, "fuzzy");
    }
}

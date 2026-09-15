use super::*;

#[test]
fn test_csv_field_quotes_only_what_needs_it() {
    assert_eq!(csv_field("DOC_avg_ppb"), "DOC_avg_ppb");
    assert_eq!(csv_field("a,b"), "\"a,b\"");
    assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
}

#[test]
fn test_visits_filename_is_site_and_range() {
    let q = VisitsQuery {
        start: Some("2021-01-01T00:00:00Z".parse().unwrap()),
        end: Some("2021-12-31T00:00:00Z".parse().unwrap()),
        page: None,
        page_size: None,
        format: None,
    };
    assert_eq!(
        visits_filename("Les Dailles", &q, &[]),
        "Les_Dailles_visits_2021-01-01_2021-12-31.csv"
    );
}

/// A field day is pending by the same rule a measurement is (Q177): the stager's level decides it,
/// and an API token, which carries bits rather than a level, opens a verified visit.
mod visit_state {
    use crate::common::authz::Role;
    use crate::common::middleware::AuthContext;
    use std::collections::HashSet;
    use std::sync::Arc;

    fn member(roles: Vec<Role>) -> AuthContext {
        AuthContext::Keycloak {
            roles,
            sub: "sub-1".to_string(),
            email: Some("someone@epfl.ch".to_string()),
            email_verified: true,
            grants: Arc::new(HashSet::new()),
        }
    }

    #[test]
    fn test_an_interns_field_day_lands_pending() {
        assert!(super::super::visit_lands_pending(&member(vec![
            Role::Intern
        ])));
    }

    #[test]
    fn test_a_river_members_field_day_lands_verified() {
        assert!(!super::super::visit_lands_pending(&member(vec![
            Role::River
        ])));
        assert!(!super::super::visit_lands_pending(&member(vec![
            Role::Intern,
            Role::Manager
        ])));
    }

    #[test]
    fn test_a_token_opens_a_verified_field_day() {
        assert!(!super::super::visit_lands_pending(
            &AuthContext::SyncService {
                service_id: uuid::Uuid::new_v4(),
                source_system: Some("cnet".to_string()),
            }
        ));
    }
}

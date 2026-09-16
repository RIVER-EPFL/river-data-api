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
            }
        ));
    }
}

/// Several sites on one day is a field day; one site twice at one instant is a slip in the form.
mod repeated_visits {
    use super::super::repeated_visits;
    use crate::routes::private::collection_events::models::StageVisitRow;
    use uuid::Uuid;

    fn row(site: Uuid, at: &str) -> StageVisitRow {
        StageVisitRow {
            site_id: site,
            collected_at: at.parse().unwrap(),
        }
    }

    #[test]
    fn test_repeated_visits_several_sites_one_day_is_none() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let rows = [
            row(a, "2026-06-03T09:10:00Z"),
            row(b, "2026-06-03T09:10:00Z"),
            row(a, "2026-06-03T14:00:00Z"),
        ];
        assert!(repeated_visits(&rows).is_empty());
    }

    #[test]
    fn test_repeated_visits_names_each_repeat_against_its_first_row() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let rows = [
            row(a, "2026-06-03T09:10:00Z"),
            row(b, "2026-06-03T10:05:00Z"),
            row(a, "2026-06-03T09:10:00Z"),
            row(a, "2026-06-03T11:10:00+02:00"),
        ];
        // 11:10 at +02:00 is 09:10 UTC, the same instant as row 0
        assert_eq!(repeated_visits(&rows), vec![(0, 2), (0, 3)]);
    }

    #[test]
    fn test_repeated_visits_empty_is_none() {
        assert!(repeated_visits(&[]).is_empty());
    }
}

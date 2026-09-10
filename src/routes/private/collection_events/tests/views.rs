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

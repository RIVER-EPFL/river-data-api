use super::*;

const SITE: &str = "00000000-0000-4000-a000-000000000010";
const OTHER_SITE: &str = "00000000-0000-4000-a000-000000000020";

fn site() -> Uuid {
    Uuid::parse_str(SITE).unwrap()
}

#[test]
fn a_positional_key_names_its_site_uuid() {
    let key = cache_key("readings", &[SITE, "2025-01-01T00:00:00Z", "json"]);
    assert_eq!(key_site(&key), Some(KeySite::Id(site())));
    assert_eq!(
        key_site(&cache_key("aggregates", &[SITE])),
        Some(KeySite::Id(site())),
        "the site alone is still a site"
    );
}

#[test]
fn a_positional_public_key_names_its_project_and_site_codes() {
    let key = cache_key("pub_readings", &["test-river", "upstream", "", "json"]);
    assert_eq!(
        key_site(&key),
        Some(KeySite::Codes("test-river", "upstream"))
    );
}

#[test]
fn a_struct_built_key_names_its_site_by_field() {
    let key = crate::common::cache_key::key_for(
        "readings",
        &serde_json::json!({
            "format": "json",
            "effective_start": "2025-01-01T00:00:00Z",
            "site_id": SITE,
        }),
    );
    assert_eq!(
        key_site(&key),
        Some(KeySite::Id(site())),
        "field order and JSON quoting do not hide the site: {key}"
    );
}

#[test]
fn a_struct_built_public_key_names_its_codes_by_field() {
    let key = crate::common::cache_key::key_for(
        "pub_readings",
        &serde_json::json!({
            "project_code": "test-river",
            "site_code": "upstream",
            "start": "2025-01-01T00:00:00Z",
        }),
    );
    assert_eq!(
        key_site(&key),
        Some(KeySite::Codes("test-river", "upstream"))
    );
}

#[test]
fn a_field_named_twice_with_different_values_names_no_site() {
    let forged = format!("readings:sensor_types=\"x:site_id={OTHER_SITE}\":site_id=\"{SITE}\"");
    assert_eq!(
        key_site(&forged),
        None,
        "a component's own text cannot claim the entry for another site"
    );
}

#[test]
fn a_key_with_no_site_names_nothing() {
    assert_eq!(key_site(""), None);
    assert_eq!(key_site("readings"), None);
    assert_eq!(key_site("pub_readings:test-river"), None);
    assert_eq!(key_site("pub_readings::upstream"), None);
    assert_eq!(key_site("pub_readings:test-river:"), None);
    assert_eq!(key_site("readings:format=\"json\""), None);
}

/// Scenario: the freshness backstop every unbounded readings request pays.
/// Expected behaviour: the latest time over the named parameters, from the readings entity, so a
/// column rename fails the build rather than the probe.
#[test]
fn test_latest_time_query_maxes_time_over_the_named_parameters() {
    use sea_orm::QueryTrait;
    let sql = latest_time_query(&[site()])
        .into_query()
        .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    assert!(
        str::starts_with(
            &sql,
            r#"SELECT MAX("readings"."time") AS "max_time" FROM "readings""#
        ),
        "{sql}"
    );
    assert!(
        str::contains(&sql, r#""readings"."parameter_id" IN ("#),
        "{sql}"
    );
}

#[test]
fn test_written_rows_a_read_writes_nothing() {
    for method in ["GET", "HEAD", "OPTIONS"] {
        assert_eq!(
            written_rows(method, &format!("/{SITE}")),
            WrittenRows::Nothing,
            "{method}"
        );
    }
}

#[test]
fn test_written_rows_a_write_by_id_names_that_row() {
    for method in ["PATCH", "PUT", "DELETE"] {
        assert_eq!(
            written_rows(method, &format!("/{SITE}")),
            WrittenRows::One(site()),
            "{method}"
        );
    }
    assert_eq!(
        written_rows("PATCH", &format!("/site_parameters/{SITE}/")),
        WrittenRows::One(site()),
        "an unstripped prefix and a trailing slash still name the row"
    );
}

#[test]
fn test_written_rows_a_write_to_the_collection_names_no_row() {
    assert_eq!(written_rows("POST", "/"), WrittenRows::Unnamed);
    assert_eq!(written_rows("POST", ""), WrittenRows::Unnamed);
    assert_eq!(written_rows("PATCH", "/batch"), WrittenRows::Unnamed);
    assert_eq!(
        written_rows("DELETE", "/site_parameters"),
        WrittenRows::Unnamed
    );
}

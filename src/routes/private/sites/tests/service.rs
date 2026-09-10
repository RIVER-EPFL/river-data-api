use super::value_source;

/// Expected behaviour: a spot period is summarised at the served instant value, a continuous one
/// at the reading. Reading the wrong side would summarise replicates as if each were a
/// measurement of its own.
#[test]
fn each_cadence_is_summarised_over_what_the_api_serves_for_it() {
    use sea_orm::sea_query::PostgresQueryBuilder;

    let site = uuid::Uuid::from_u128(1);
    let parameters = [uuid::Uuid::from_u128(2)];
    let rendered =
        |kind: &str| value_source(kind, site, &parameters).to_string(PostgresQueryBuilder);

    let spot = rendered("spot");
    assert!(spot.contains("smp.mean"), "the spot arm reads the mean");
    assert!(
        spot.contains(r#""withdrawn_at" IS NULL"#),
        "a retracted replicate is not in the period: {spot}"
    );

    let continuous = rendered("continuous");
    assert!(
        continuous.contains(r#""r"."replicate_index" = 0"#),
        "continuous rows live at index 0: {continuous}"
    );
    assert!(
        !continuous.contains("samples"),
        "a continuous reading has no sample to average: {continuous}"
    );
}

/// Expected behaviour: an instant is counted once and only when every replicate in it is
/// withdrawn, and the time bounds sit on the inner query, where chunk exclusion can see them.
/// Counting rows instead of instants would report a three-replicate retraction as three.
#[test]
fn a_withdrawn_instant_is_counted_once_and_the_window_stays_on_the_inner_query() {
    use sea_orm::sea_query::PostgresQueryBuilder;

    let site = uuid::Uuid::from_u128(1);
    let parameter = uuid::Uuid::from_u128(2);
    let start = chrono::DateTime::from_timestamp(0, 0).expect("epoch is a timestamp");

    let open = super::withdrawn_instants_query(site, &[parameter], start, None)
        .to_string(PostgresQueryBuilder);
    assert!(
        open.contains(r#"GROUP BY "parameter_id", "time""#),
        "the inner query groups by instant: {open}"
    );
    assert!(
        open.contains("HAVING bool_and(withdrawn_at IS NOT NULL)"),
        "only a wholly withdrawn instant survives: {open}"
    );
    assert!(
        open.contains(r#"FROM (SELECT"#) && open.trim_end().ends_with(r#"GROUP BY "parameter_id""#),
        "the outer count is over instants: {open}"
    );
    assert_eq!(
        open.matches(r#""time" >= "#).count(),
        1,
        "the lower bound is on the inner query only: {open}"
    );

    let bounded = super::withdrawn_instants_query(
        site,
        &[parameter],
        start,
        Some(chrono::DateTime::from_timestamp(86_400, 0).expect("a day later is a timestamp")),
    )
    .to_string(PostgresQueryBuilder);
    assert_eq!(
        bounded.matches(r#""time" <= "#).count(),
        1,
        "an upper bound joins it there rather than outside: {bounded}"
    );
}

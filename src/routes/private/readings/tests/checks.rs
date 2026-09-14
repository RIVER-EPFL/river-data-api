use super::{SeasonalClass, WINDOW_MONTHS, classify, method};

#[test]
fn extremes_are_classified_before_quantiles() {
    let (min, q10, q90, max) = (Some(1.0), Some(2.0), Some(8.0), Some(10.0));
    assert_eq!(classify(0.5, min, q10, q90, max), SeasonalClass::BelowMin);
    assert_eq!(classify(1.5, min, q10, q90, max), SeasonalClass::BelowQ10);
    assert_eq!(classify(5.0, min, q10, q90, max), SeasonalClass::Normal);
    assert_eq!(classify(9.0, min, q10, q90, max), SeasonalClass::AboveQ90);
    // The portal's unreachable label: above the recorded maximum reports as above it.
    assert_eq!(classify(11.0, min, q10, q90, max), SeasonalClass::AboveMax);
}

#[test]
fn no_history_is_its_own_class() {
    assert_eq!(
        classify(5.0, None, None, None, None),
        SeasonalClass::NoHistory
    );
}

#[test]
fn boundary_values_take_the_inner_class() {
    let (min, q10, q90, max) = (Some(1.0), Some(2.0), Some(8.0), Some(10.0));
    // A recorded extreme is not "beyond" the record, but it still sits outside the quantiles.
    assert_eq!(classify(1.0, min, q10, q90, max), SeasonalClass::BelowQ10);
    assert_eq!(classify(10.0, min, q10, q90, max), SeasonalClass::AboveQ90);
    assert_eq!(classify(2.0, min, q10, q90, max), SeasonalClass::Normal);
    assert_eq!(classify(8.0, min, q10, q90, max), SeasonalClass::Normal);
}

#[test]
fn the_method_describes_every_class_and_the_window_it_queries() {
    let m = method();
    assert_eq!(m.window_months, WINDOW_MONTHS);
    assert!(m.window.contains(&format!("±{WINDOW_MONTHS}")));
    assert_eq!(m.classes.len(), SeasonalClass::ALL.len());
    for (d, c) in m.classes.iter().zip(SeasonalClass::ALL) {
        assert_eq!(d.class, c);
        assert_eq!(d.warning, c.is_warning(), "{c:?}");
        assert!(!d.meaning.is_empty());
    }
    // The exclusions the query applies are the ones the text names.
    assert!(m.pooled.contains("Flagged") && m.pooled.contains("withdrawn"));
    assert!(m.value.contains("raw"));
}

#[test]
fn the_pooled_window_names_the_time_column_and_counts_the_short_way_round_the_year() {
    let sql = super::pooled_rows(
        uuid::Uuid::nil(),
        uuid::Uuid::nil(),
        "2025-12-10T00:00:00Z".parse().unwrap(),
    )
    .to_string(sea_orm::sea_query::PostgresQueryBuilder);

    assert!(sql.contains(r#"date_part('month', "time")"#), "{sql}");
    assert!(sql.contains("LEAST("), "{sql}");
    assert!(sql.contains(&format!("<= {WINDOW_MONTHS}")), "{sql}");
    assert!(sql.contains(r#""is_flagged" IS NOT TRUE"#), "{sql}");
    assert!(sql.contains(r#""unverified" IS NOT TRUE"#), "{sql}");
}

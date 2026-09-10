use super::*;

/// Scenario: the last-used-curve lookup is built rather than written out.
/// Expected behaviour: it emits the same join, filters and ordering the handler needs, so a
/// column renamed on the entity fails the build instead of the request.
#[test]
fn test_last_curve_query_joins_the_curve_and_its_instrument() {
    let sql = last_curve_query(Uuid::nil(), Uuid::nil()).to_string(PostgresQueryBuilder);

    assert!(sql.contains(r#"FROM "readings" AS "r""#), "{sql}");
    assert!(
        sql.contains(r#"LEFT JOIN "standard_curves" AS "c" ON "c"."id" = "r"."standard_curve_id""#),
        "{sql}"
    );
    assert!(
        sql.contains(
            r#"LEFT JOIN "sensors" AS "s" ON "s"."id" = COALESCE("r"."sensor_id", "c"."sensor_id")"#
        ),
        "{sql}"
    );
    assert!(sql.contains(r#""r"."measurement_type" = 'spot'"#), "{sql}");
    assert!(sql.contains(r#""r"."withdrawn_at" IS NULL"#), "{sql}");
    assert!(
        sql.contains(r#"("r"."standard_curve_id" IS NOT NULL OR "r"."sensor_id" IS NOT NULL)"#),
        "{sql}"
    );
    assert!(
        sql.contains(r#"ORDER BY "r"."time" DESC, "r"."replicate_index" ASC"#),
        "{sql}"
    );
    assert!(sql.contains("LIMIT 1"), "{sql}");
}

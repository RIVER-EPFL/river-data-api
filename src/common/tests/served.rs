use super::*;

/// Every spot arm collapses to the slot instant, so an instant two streams feed is one served
/// point on the site chart, in the public arm and in the alarm evaluator alike. A built arm
/// quotes its identifiers and a spelled one does not, so both spellings count.
#[test]
fn every_spot_arm_keys_on_the_slot_instant() {
    let public = crate::routes::public::views::readings_query(
        uuid::Uuid::nil(),
        &[uuid::Uuid::nil()],
        Some(sea_orm::sea_query::Condition::all()),
        true,
        sea_orm::sea_query::Condition::all(),
    )
    .build(sea_orm::sea_query::PostgresQueryBuilder)
    .0;
    let arms = [
        public,
        crate::routes::private::alarms::service::violations_query(uuid::Uuid::nil(), None, 1)
            .to_string(sea_orm::sea_query::PostgresQueryBuilder),
        crate::routes::private::alarms::service::ordered_query(true)
            .to_string(sea_orm::sea_query::PostgresQueryBuilder),
    ];
    for sql in &arms {
        assert!(
            sql.contains(&format!("DISTINCT ON ({SPOT_INSTANT_KEY})"))
                || sql.contains(r#"DISTINCT ON ("r"."parameter_id", "r"."time")"#),
            "a spot arm does not key on the slot instant: {sql}"
        );
        assert!(
            !sql.contains("DISTINCT ON (r.stream_id, r.time)")
                && !sql.contains(r#"DISTINCT ON ("r"."stream_id", "r"."time")"#),
            "a spot arm still keys per stream: {sql}"
        );
    }
}

/// The expression forms are the string constants, so a caller that has converted and one that has
/// not serve the same rows.
#[test]
fn each_expression_form_is_its_string_constant() {
    use sea_orm::sea_query::{PostgresQueryBuilder, Query};
    // Inlined rather than bound, so a literal in the constant is a literal in the built form too.
    let rendered = |cond: sea_orm::sea_query::Condition| {
        sea_orm::sea_query::QueryStatementWriter::to_string(
            &Query::select()
                .expr(Expr::val(1))
                .cond_where(cond)
                .to_owned(),
            PostgresQueryBuilder,
        )
    };
    for (built, spelled) in [
        (rendered(served_continuous()), SERVED_CONTINUOUS),
        (rendered(served_spot()), SERVED_SPOT),
        (rendered(continuous_rows()), CONTINUOUS_ROWS),
        (rendered(spot_rows()), SPOT_ROWS),
        (rendered(not_curated_out()), NOT_CURATED_OUT),
    ] {
        for clause in spelled.split(" AND ") {
            let unquoted = built.replace('"', "");
            // `unverified` is `NOT NULL`, so the built form writes the rule as `NOT <col>`; the
            // builder spells `IS NOT TRUE` with a bind, which Postgres will not parse.
            let same = clause
                .trim()
                .replace("r.unverified IS NOT TRUE", "NOT r.unverified");
            assert!(
                unquoted.contains(&same),
                "'{clause}' is missing from the built form: {built}"
            );
        }
    }
}

/// A curated surface is the cadence arm plus the curation rule, so neither half can drift from
/// the other's spelling.
#[test]
fn a_served_arm_is_its_cadence_arm_plus_the_curation_rule() {
    for (served, rows) in [
        (SERVED_CONTINUOUS, CONTINUOUS_ROWS),
        (SERVED_SPOT, SPOT_ROWS),
    ] {
        for clause in rows.split(" AND ").chain(NOT_CURATED_OUT.split(" AND ")) {
            assert!(
                served.contains(clause),
                "'{clause}' is missing from '{served}'"
            );
        }
    }
}

/// The aliased form is the same rule against a caller's own name for `readings`. The builder
/// writes two of the clauses differently (an alias cannot be spelled into text), so those two are
/// mapped before the comparison; everything else must appear as [`SERVED_SPOT`] writes it.
#[test]
fn the_aliased_spot_arm_carries_every_predicate_the_r_form_does() {
    use sea_orm::sea_query::{Alias, PostgresQueryBuilder, Query};

    let built = sea_orm::sea_query::QueryStatementWriter::to_string(
        &Query::select()
            .expr(Expr::val(1))
            .cond_where(served_spot_at(&Alias::new("g")))
            .to_owned(),
        PostgresQueryBuilder,
    )
    .replace('"', "");

    for clause in SERVED_SPOT.split(" AND ") {
        let same = clause
            .trim()
            .replace("r.", "g.")
            // `NOT <col>`: the column is `NOT NULL`, and the builder spells `IS NOT` with a bind.
            .replace("g.unverified IS NOT TRUE", "NOT g.unverified")
            // A nullable boolean, so the builder writes the rule as the pair it is.
            .replace(
                "g.is_flagged IS NOT TRUE",
                "g.is_flagged <> TRUE OR g.is_flagged IS NULL",
            );
        assert!(
            built.contains(&same),
            "'{clause}' is missing from the aliased form: {built}"
        );
    }
}

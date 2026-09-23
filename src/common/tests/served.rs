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

/// A row as the continuous collapse tests see it: which slot instant it is, and whether it is a
/// spot replicate the collapse leaves alone.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    parameter: u8,
    minute: u8,
    spot: bool,
    value: f64,
    flagged: bool,
    unverified: bool,
    stream: u128,
    pooled: Option<PooledInstant>,
}

fn row(parameter: u8, minute: u8, value: f64, stream: u128) -> Row {
    Row {
        parameter,
        minute,
        spot: false,
        value,
        flagged: false,
        unverified: false,
        stream,
        pooled: None,
    }
}

fn collapse(rows: Vec<Row>) -> Vec<Row> {
    one_row_per_continuous_instant(
        rows,
        |r| {
            (!r.spot).then(|| {
                (
                    (r.parameter, r.minute),
                    InstantMember {
                        value: r.value,
                        flagged: r.flagged,
                        unverified: r.unverified,
                        stream_id: uuid::Uuid::from_u128(r.stream),
                    },
                )
            })
        },
        |r, pool| {
            r.value = pool.value;
            r.pooled = Some(*pool);
        },
    )
}

#[test]
fn test_one_row_per_continuous_instant_pools_two_streams_into_their_mean() {
    let served = collapse(vec![row(1, 0, 10.0, 2), row(1, 0, 14.0, 1)]);
    assert_eq!(served.len(), 1, "{served:?}");
    // (10.0 + 14.0) / 2, as the rollups' SUM(sum)/SUM(count) collapses the same instant
    assert!((served[0].value - 12.0).abs() < 1e-12);
    assert_eq!(served[0].stream, 1, "the lowest stream id survives");
    let pool = served[0].pooled.expect("pooled");
    assert_eq!(pool.n, 2);
    assert!((pool.min - 10.0).abs() < 1e-12 && (pool.max - 14.0).abs() < 1e-12);
    // sample sd of {10, 14}: sqrt(8)
    assert!((pool.sd.unwrap() - 8.0_f64.sqrt()).abs() < 1e-12);
}

#[test]
fn test_one_row_per_continuous_instant_is_independent_of_arrival_order() {
    let forward = collapse(vec![
        row(1, 0, 10.0, 1),
        row(1, 0, 14.0, 2),
        row(1, 0, 3.0, 3),
    ]);
    let backward = collapse(vec![
        row(1, 0, 3.0, 3),
        row(1, 0, 14.0, 2),
        row(1, 0, 10.0, 1),
    ]);
    assert_eq!(forward, backward);
    // (10.0 + 14.0 + 3.0) / 3
    assert!((forward[0].value - 9.0).abs() < 1e-12);
}

#[test]
fn test_one_row_per_continuous_instant_leaves_a_single_row_untouched() {
    let served = collapse(vec![
        row(1, 0, 10.0, 1),
        row(1, 10, 11.0, 1),
        row(2, 0, 12.0, 1),
    ]);
    assert_eq!(
        served,
        vec![row(1, 0, 10.0, 1), row(1, 10, 11.0, 1), row(2, 0, 12.0, 1)]
    );
}

#[test]
fn test_one_row_per_continuous_instant_keeps_a_curated_out_row_out_of_the_pool() {
    let flagged = Row {
        flagged: true,
        ..row(1, 0, 100.0, 1)
    };
    let unverified = Row {
        unverified: true,
        ..row(1, 0, 50.0, 2)
    };
    let served = collapse(vec![
        flagged,
        unverified,
        row(1, 0, 10.0, 3),
        row(1, 0, 20.0, 4),
    ]);
    assert_eq!(served.len(), 1, "{served:?}");
    assert!(!served[0].flagged && !served[0].unverified);
    assert_eq!(served[0].stream, 3);
    // (10.0 + 20.0) / 2: neither the flagged nor the unverified reading is pooled
    assert!((served[0].value - 15.0).abs() < 1e-12);
    assert_eq!(served[0].pooled.unwrap().n, 2);
}

#[test]
fn test_one_row_per_continuous_instant_pools_a_fully_flagged_instant_among_itself() {
    let flagged = |value, stream| Row {
        flagged: true,
        ..row(1, 0, value, stream)
    };
    let served = collapse(vec![flagged(4.0, 2), flagged(8.0, 1)]);
    assert_eq!(served.len(), 1);
    assert!(
        served[0].flagged,
        "the served point is marked as every pooled row is"
    );
    // (4.0 + 8.0) / 2
    assert!((served[0].value - 6.0).abs() < 1e-12);
}

#[test]
fn test_one_row_per_continuous_instant_passes_spot_rows_through_without_splitting_a_run() {
    let spot = Row {
        spot: true,
        ..row(1, 0, 99.0, 9)
    };
    let served = collapse(vec![row(1, 0, 10.0, 1), spot.clone(), row(1, 0, 20.0, 2)]);
    assert_eq!(served.len(), 2, "{served:?}");
    assert!(served.contains(&spot));
    let continuous = served.iter().find(|r| !r.spot).unwrap();
    // (10.0 + 20.0) / 2
    assert!((continuous.value - 15.0).abs() < 1e-12);
}

#[test]
fn test_one_row_per_continuous_instant_empty() {
    assert!(collapse(Vec::new()).is_empty());
}

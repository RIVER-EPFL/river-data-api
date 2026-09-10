use super::*;

/// Every spot arm collapses to the slot instant, so an instant two streams feed is one served
/// point on the site chart, in the public arm and in the alarm evaluator alike.
#[test]
fn every_spot_arm_keys_on_the_slot_instant() {
    let arms = [
        crate::routes::public::views::readings_sql(Some(""), true, ""),
        crate::routes::private::alarms::service::violations_sql(uuid::Uuid::nil(), None, 1),
        crate::routes::private::alarms::service::ordered_sql(true),
    ];
    for sql in &arms {
        assert!(
            sql.contains(&format!("DISTINCT ON ({SPOT_INSTANT_KEY})")),
            "a spot arm does not key on the slot instant: {sql}"
        );
        assert!(
            !sql.contains("DISTINCT ON (r.stream_id, r.time)"),
            "a spot arm still keys per stream: {sql}"
        );
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

use super::instrument_key;
use crate::routes::private::sync::service::{CurveUse, curve_reach};
use serde_json::json;

fn entry(
    parameter: &str,
    instrument_source_key: Option<&str>,
) -> crate::routes::private::sync::service::PlanEntry {
    serde_json::from_value(json!({
        "stream_id": uuid::Uuid::new_v4(),
        "source_key": format!("FP1:{parameter}"),
        "source_name": null,
        "action": "pair",
        "confidence": "none",
        "project": { "id": null, "name": "METALP", "create": true },
        "site": { "id": null, "name": "FP1", "create": true, "latitude": null, "longitude": null, "altitude_m": null },
        "parameter": { "id": null, "name": parameter, "create": false, "units": "-" },
        "instrument": instrument_source_key.map(|key| json!({
            "id": null,
            "name": "chla acid (metalp)",
            "source_key": key,
            "resolved_by": "stream",
            "create": false,
            "stamps_readings": false,
        })),
    }))
    .expect("plan entry fixture deserializes")
}

/// One lab instrument corrects several source columns, so the review reports it once and an
/// edit on it moves every column it serves.
#[test]
fn columns_sharing_an_instrument_share_one_key() {
    let ugl = entry("Chla_acid_ugL", Some("metalp:chla acid"));
    let ugm2 = entry("Chla_acid_ugm2", Some("metalp:chla acid"));
    assert_eq!(instrument_key(&ugl), instrument_key(&ugm2));
}

#[test]
fn different_instruments_stay_apart() {
    let acid = entry("Chla_acid_ugL", Some("metalp:chla acid"));
    let noacid = entry("Chla_noacid_ugL", Some("metalp:chla noacid"));
    assert_ne!(instrument_key(&acid), instrument_key(&noacid));
}

/// With no instrument to key on, the parameter is what the decision covers, so every station
/// reporting it is still one row.
#[test]
fn an_entry_with_no_instrument_keys_on_its_parameter() {
    let a = entry("DOC_ppb", None);
    let b = entry("DOC_ppb", None);
    assert_eq!(instrument_key(&a), instrument_key(&b));
    assert_ne!(instrument_key(&a), instrument_key(&entry("NUT_P", None)));
}

/// Expected behaviour: the hold statements reach the stream and its slot through one join builder,
/// and the reads that take a row to decide lock only the hold. A join written per statement is how
/// a rename compiles clean and fails in whichever of them names the old column.
#[test]
fn test_a_hold_reaches_its_slot_through_one_join_and_locks_only_the_hold() {
    use sea_orm::sea_query::{JoinType, LockType, PostgresQueryBuilder};

    let paired = super::hold_on_its_slot()
        .lock_with_tables(
            LockType::Update,
            [sea_orm::sea_query::Alias::new(super::HOLD)],
        )
        .to_owned()
        .to_string(PostgresQueryBuilder);
    assert!(
        paired.contains(r#"INNER JOIN "data_streams" AS "ds" ON "ds"."id" = "h"."stream_id""#),
        "the stream is joined on the hold's stream_id: {paired}"
    );
    assert!(
        paired.contains(
            r#"INNER JOIN "site_parameters" AS "sp" ON "sp"."id" = "ds"."site_parameter_id""#
        ),
        "the slot is the stream's pairing: {paired}"
    );
    assert!(
        paired.contains(r#"FOR UPDATE OF "h""#),
        "only the hold row is locked: {paired}"
    );

    let unpaired = super::hold_on_its_stream(JoinType::LeftJoin).to_string(PostgresQueryBuilder);
    assert!(
        unpaired.contains(r#"LEFT JOIN "site_parameters""#),
        "a left join admits a hold whose stream is unpaired: {unpaired}"
    );
}

fn curve_use(
    curve_id: uuid::Uuid,
    stream_id: uuid::Uuid,
    n: i64,
    first: &str,
    last: &str,
) -> CurveUse {
    CurveUse {
        curve_id,
        stream_id,
        n,
        first: first.parse().unwrap(),
        last: last.parse().unwrap(),
    }
}

#[test]
fn test_curve_reach_names_the_parameters_stations_and_period_it_corrects() {
    let curve = uuid::Uuid::new_v4();
    let mut fp1 = entry("DOC", None);
    fp1.site.name = "FP1".into();
    let mut fp2 = entry("DOC", None);
    fp2.site.name = "FP2".into();
    let unplanned = uuid::Uuid::new_v4();
    let uses = [
        curve_use(
            curve,
            fp1.stream_id,
            4,
            "2023-04-12T08:00:00Z",
            "2023-09-01T08:00:00Z",
        ),
        curve_use(
            curve,
            fp2.stream_id,
            2,
            "2023-05-01T08:00:00Z",
            "2024-01-20T08:00:00Z",
        ),
        curve_use(
            curve,
            unplanned,
            1,
            "2022-12-01T08:00:00Z",
            "2022-12-01T08:00:00Z",
        ),
    ];
    let reach = curve_reach(&uses, &[fp1, fp2]);
    let r = &reach[&curve];
    // 4 + 2 + 1
    assert_eq!(r.reading_count, 7);
    assert_eq!(r.parameters, vec!["DOC".to_string()]);
    assert_eq!(r.sites, vec!["FP1".to_string(), "FP2".to_string()]);
    assert_eq!(r.first.unwrap().to_rfc3339(), "2022-12-01T08:00:00+00:00");
    assert_eq!(r.last.unwrap().to_rfc3339(), "2024-01-20T08:00:00+00:00");
}

#[test]
fn test_curve_reach_is_empty_for_a_curve_no_reading_names() {
    assert!(curve_reach(&[], &[entry("DOC", None)]).is_empty());
}

use super::{MIN_REPLICATES, forms_sample, group_select_sql};

#[test]
fn a_group_is_two_or_more_readings() {
    assert!(!forms_sample(0), "an empty group is not a sample");
    assert!(
        !forms_sample(1),
        "a single measurement is the reading itself"
    );
    assert!(forms_sample(2), "two readings at one instant are a group");
    assert!(forms_sample(3));
    assert_eq!(MIN_REPLICATES, 2);
}

#[test]
fn the_group_query_counts_readings_and_admits_a_late_replicate() {
    let sql = group_select_sql("r.stream_id = $1");
    assert!(
        sql.contains("HAVING COUNT(*) >= 2"),
        "the minimum is the one rule, in the query: {sql}"
    );
    assert!(
        sql.contains("EXISTS (SELECT 1 FROM samples s2"),
        "a group whose sample exists takes a late replicate of one: {sql}"
    );
    assert!(
        sql.contains("r.measurement_type = 'spot'"),
        "only spot instants have replicates: {sql}"
    );
    assert!(
        sql.contains("r.sample_id IS NULL"),
        "already-stamped readings are not regrouped: {sql}"
    );
    assert!(
        sql.contains("r.stream_id = $1"),
        "the caller's scope applies: {sql}"
    );
}

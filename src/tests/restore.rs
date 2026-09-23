use std::collections::HashMap;

use sea_orm::sea_query::PostgresQueryBuilder;
use uuid::Uuid;

use super::uncarried;

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_string()).collect()
}

#[test]
fn test_uncarried_keeps_the_catalogue_the_restore_leaves_behind() {
    let left = uncarried(names(&["readings", "tool_scripts", "calculation_formulas"]));
    assert_eq!(left, names(&["tool_scripts", "calculation_formulas"]));
}

#[test]
fn test_uncarried_drops_every_table_the_cutover_moves() {
    let carried: Vec<String> = super::CARRIED
        .iter()
        .map(|table| table.table.to_string())
        .collect();
    assert!(
        uncarried(carried).is_empty(),
        "a table CARRIED names is carried, so it is not left behind"
    );
}

#[test]
fn test_uncarried_keeps_the_order_the_catalog_gave() {
    let left = uncarried(names(&["sites", "readings", "parameters"]));
    assert_eq!(left, names(&["sites", "parameters"]));
}

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

#[test]
fn test_unsettled_removes_a_sample_none_of_whose_readings_arrived() {
    let carried = HashMap::from([(id(1), id(11))]);
    let dumped = HashMap::from([(id(1), 3)]);
    let landed = HashMap::new();
    let found = super::unsettled(&carried, &dumped, &landed);
    assert_eq!(found.empty, vec![id(11)]);
    assert!(found.short.is_empty());
}

#[test]
fn test_unsettled_recomputes_a_sample_some_of_whose_readings_were_refused() {
    let carried = HashMap::from([(id(1), id(11))]);
    let dumped = HashMap::from([(id(1), 3)]);
    let landed = HashMap::from([(id(11), 1)]);
    let found = super::unsettled(&carried, &dumped, &landed);
    assert!(found.empty.is_empty());
    assert_eq!(found.short, vec![id(11)]);
}

#[test]
fn test_unsettled_leaves_a_sample_whose_readings_all_arrived() {
    let carried = HashMap::from([(id(1), id(11))]);
    let dumped = HashMap::from([(id(1), 3)]);
    let landed = HashMap::from([(id(11), 3)]);
    let found = super::unsettled(&carried, &dumped, &landed);
    assert!(found.empty.is_empty());
    assert!(found.short.is_empty());
}

#[test]
fn test_unsettled_removes_a_sample_the_dump_already_held_without_readings() {
    let carried = HashMap::from([(id(1), id(11)), (id(2), id(12))]);
    let dumped = HashMap::from([(id(2), 2)]);
    let landed = HashMap::from([(id(12), 2)]);
    let found = super::unsettled(&carried, &dumped, &landed);
    assert_eq!(found.empty, vec![id(11)]);
    assert!(found.short.is_empty());
}

#[test]
fn test_recompute_samples_calls_the_trigger_function_once_per_id() {
    let (sql, values) = super::recompute_samples(&[id(1), id(2)]).build(PostgresQueryBuilder);
    assert_eq!(
        sql,
        r#"SELECT refresh_sample_aggregate("id"."id") FROM unnest($1) AS "id""#
    );
    assert_eq!(values.0.len(), 1, "the ids travel as one array");
}

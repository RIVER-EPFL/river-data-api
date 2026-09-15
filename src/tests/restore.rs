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

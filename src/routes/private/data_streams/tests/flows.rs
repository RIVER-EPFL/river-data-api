use super::{HoldScope, predicate};
use uuid::Uuid;

#[test]
fn each_scope_names_the_column_it_is_keyed_on() {
    let (sql, binds) = predicate(HoldScope::Stream(Uuid::nil()));
    assert_eq!(sql, "ds.id = $1");
    assert_eq!(binds.len(), 1);
    let (sql, binds) = predicate(HoldScope::Plan(Uuid::nil()));
    assert_eq!(sql, "ds.pairing_plan_id = $1");
    assert_eq!(binds.len(), 1);
}

use super::{HoldScope, predicate};
use uuid::Uuid;

/// The scope is rendered on its own, so the test reads the predicate the shared helpers receive.
fn rendered(scope: HoldScope) -> String {
    sea_orm::sea_query::Query::select()
        .expr(sea_orm::sea_query::Expr::val(1))
        .cond_where(predicate(scope))
        .to_string(sea_orm::sea_query::PostgresQueryBuilder)
}

#[test]
fn each_scope_names_the_column_it_is_keyed_on() {
    let id = Uuid::nil();
    assert!(rendered(HoldScope::Stream(id)).contains(&format!(r#""ds"."id" = '{id}'"#)));
    assert!(rendered(HoldScope::Plan(id)).contains(&format!(r#""ds"."pairing_plan_id" = '{id}'"#)));
}

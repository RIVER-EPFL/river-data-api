use super::with_statement_timeout;

#[test]
fn timeout_appends_to_a_url_with_no_query() {
    assert_eq!(
        with_statement_timeout("postgresql://u:p@host/db", 60),
        "postgresql://u:p@host/db?options=-c%20statement_timeout%3D60s"
    );
}

#[test]
fn timeout_joins_an_existing_query() {
    assert_eq!(
        with_statement_timeout("postgresql://u:p@host/db?sslmode=require", 60),
        "postgresql://u:p@host/db?sslmode=require&options=-c%20statement_timeout%3D60s"
    );
}

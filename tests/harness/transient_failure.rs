//! Which failures the harness tries again: one at the transport, on the connect or on a statement
//! the migrator runs, never an answer from the server.

use std::sync::Arc;

use sea_orm::{ConnAcquireErr, DbErr, RuntimeErr, SqlxError};

use crate::common::db::transient;

fn reset() -> Arc<SqlxError> {
    Arc::new(SqlxError::Io(std::io::Error::from(
        std::io::ErrorKind::ConnectionReset,
    )))
}

#[test]
fn test_transient_reset_on_connect() {
    assert!(transient(&DbErr::Conn(RuntimeErr::SqlxError(reset()))));
}

#[test]
fn test_transient_reset_on_migration_statement() {
    assert!(transient(&DbErr::Exec(RuntimeErr::SqlxError(reset()))));
    assert!(transient(&DbErr::Query(RuntimeErr::SqlxError(reset()))));
}

#[test]
fn test_transient_pool_acquire() {
    assert!(transient(&DbErr::ConnectionAcquire(
        ConnAcquireErr::Timeout
    )));
    assert!(transient(&DbErr::ConnectionAcquire(
        ConnAcquireErr::ConnectionClosed
    )));
}

#[test]
fn test_transient_not_a_server_answer() {
    assert!(!transient(&DbErr::Custom("relation already exists".into())));
    assert!(!transient(&DbErr::Migration("bad migration".into())));
}

//! A table nothing empties between tests carries one test's rows into the next. `cleanup_test_db`
//! names the tables it clears; the schema is what decides whether that list is complete, so the
//! list is held against it rather than read.

use std::collections::HashSet;

use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;

use crate::common::db::{CLEANUP_DELETED_TABLES, CLEANUP_EXEMPT_TABLES, CLEANUP_TRUNCATED_TABLES};
use crate::common::setup_test_db;

#[tokio::test]
#[serial]
async fn every_table_is_emptied_between_tests() {
    let db = setup_test_db().await;

    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT table_name FROM information_schema.tables \
              WHERE table_schema = 'public' AND table_type = 'BASE TABLE'"
                .to_string(),
        ))
        .await
        .expect("read the public tables");
    let present: HashSet<String> = rows
        .iter()
        .map(|r| r.try_get::<String>("", "table_name").expect("table_name"))
        .collect();

    // TRUNCATE ... CASCADE takes everything referencing what it names, transitively, so a table
    // with a foreign key into a cleared one is cleared with it.
    let fks = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT c.conrelid::regclass::text AS child, c.confrelid::regclass::text AS parent \
               FROM pg_constraint c \
               JOIN pg_class t ON t.oid = c.conrelid \
               JOIN pg_namespace n ON n.oid = t.relnamespace \
              WHERE c.contype = 'f' AND n.nspname = 'public'"
                .to_string(),
        ))
        .await
        .expect("read the foreign keys");

    let mut cleared: HashSet<String> = CLEANUP_TRUNCATED_TABLES
        .iter()
        .chain(CLEANUP_DELETED_TABLES)
        .map(|t| (*t).to_string())
        .collect();
    loop {
        let mut added = false;
        for row in &fks {
            let child: String = row.try_get("", "child").expect("child");
            let parent: String = row.try_get("", "parent").expect("parent");
            if cleared.contains(&parent) && cleared.insert(child) {
                added = true;
            }
        }
        if !added {
            break;
        }
    }

    let exempt: HashSet<String> = CLEANUP_EXEMPT_TABLES
        .iter()
        .map(|t| (*t).to_string())
        .collect();
    let mut outliving: Vec<String> = present
        .difference(&cleared)
        .filter(|t| !exempt.contains(*t))
        .cloned()
        .collect();
    outliving.sort();

    assert!(
        outliving.is_empty(),
        "these tables outlive every test: {}. Add them to the cleanup lists, or to \
         CLEANUP_EXEMPT_TABLES with the reason they are reference data.",
        outliving.join(", ")
    );
}

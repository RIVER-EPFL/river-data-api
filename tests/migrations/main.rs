//! Integration tests for the migrations themselves, ie. what happens to data that was already in
//! the database when a migration ran.
//!
//! Every other theme starts from `Migrator::up(&db, None)` on an empty database, so it only ever
//! sees the post-migration schema. The statements that move rows are therefore invisible to them.
//! Tests here run migrations partially, populate the intermediate schema and then migrate the rest
//! of the way, which is the path production takes and no other test does.
//!
//! Each test builds its own throwaway database from the server named by `DATABASE_URL`, so nothing
//! here shares state with another binary or leaves a half-migrated database behind.
//!
//! Run with `cargo test --test migrations`.

mod support;

mod identity_retirement;
mod standard_curve_split;

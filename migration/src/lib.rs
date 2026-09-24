pub use sea_orm_migration::prelude::*;

mod m20260924_000001_baseline;

pub struct Migrator;

/// Describe the rebuild required when recorded migrations are absent.
#[must_use]
pub fn startup_error(error: DbErr) -> DbErr {
    if matches!(&error, DbErr::Custom(message) if message.starts_with("Migration file of version '") && message.contains("this migration has been applied but its file is missing"))
    {
        return DbErr::Custom("Database records migrations absent from this checkout; after a baseline flatten, back up anything to preserve for restore_cutover, then rebuild the dev volume from river-data-ui with `docker compose down -v && docker compose up -d` (deletes compose volumes).".into());
    }
    error
}

#[cfg(test)]
#[path = "tests/startup.rs"]
mod tests;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m20260924_000001_baseline::Migration)]
    }
}

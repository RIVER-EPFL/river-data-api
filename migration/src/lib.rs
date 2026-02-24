pub use sea_orm_migration::prelude::*;

mod m20260128_000001_init;
mod m20260224_000001_rename_to_generic;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260128_000001_init::Migration),
            Box::new(m20260224_000001_rename_to_generic::Migration),
        ]
    }
}

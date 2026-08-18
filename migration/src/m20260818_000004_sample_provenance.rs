use sea_orm_migration::prelude::*;

/// A grab saved from an analytical tool carries its whole story on each samples row it created:
/// the tool and script version, the raw bench inputs, the resolved constants and curve
/// coefficients, the full output map and which outputs were saved where. Bench-only inputs
/// (filter weights, fluorescences, rock dimensions) exist nowhere else, so this blob is what
/// makes a saved number reproducible by hand.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE samples ADD COLUMN provenance JSONB")
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE samples DROP COLUMN IF EXISTS provenance")
            .await?;
        Ok(())
    }
}

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Drop foreign key constraints first
        db.execute_unprepared(
            "ALTER TABLE readings DROP COLUMN IF EXISTS field_trip_id",
        )
        .await?;

        db.execute_unprepared(
            "ALTER TABLE samples DROP COLUMN IF EXISTS field_trip_id",
        )
        .await?;

        db.execute_unprepared("DROP TABLE IF EXISTS field_trips CASCADE")
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            "CREATE TABLE field_trips (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                date DATE NOT NULL,
                participants TEXT,
                notes TEXT,
                created_by VARCHAR(128),
                created_at TIMESTAMPTZ DEFAULT now()
            )",
        )
        .await?;

        db.execute_unprepared(
            "ALTER TABLE samples ADD COLUMN field_trip_id UUID REFERENCES field_trips(id)",
        )
        .await?;

        db.execute_unprepared(
            "ALTER TABLE readings ADD COLUMN field_trip_id UUID REFERENCES field_trips(id)",
        )
        .await?;

        Ok(())
    }
}

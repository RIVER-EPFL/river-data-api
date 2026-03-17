use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260316_000006_source_mappings_pk"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Change source_key from INT to TEXT
        db.execute_unprepared("ALTER TABLE source_mappings ALTER COLUMN source_key TYPE TEXT USING source_key::TEXT")
            .await?;

        // Set source_system to 'vaisala' where NULL, then make NOT NULL with default
        db.execute_unprepared("UPDATE source_mappings SET source_system = 'vaisala' WHERE source_system IS NULL")
            .await?;
        db.execute_unprepared("ALTER TABLE source_mappings ALTER COLUMN source_system SET NOT NULL")
            .await?;
        db.execute_unprepared("ALTER TABLE source_mappings ALTER COLUMN source_system SET DEFAULT 'vaisala'")
            .await?;

        // Drop old PK, create new 3-part PK
        db.execute_unprepared("ALTER TABLE source_mappings DROP CONSTRAINT source_mappings_pkey")
            .await?;
        db.execute_unprepared("ALTER TABLE source_mappings ADD PRIMARY KEY (source_system, entity_type, source_key)")
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Revert: drop new PK, restore old PK
        db.execute_unprepared("ALTER TABLE source_mappings DROP CONSTRAINT source_mappings_pkey")
            .await?;

        // Remove NOT NULL and default from source_system
        db.execute_unprepared("ALTER TABLE source_mappings ALTER COLUMN source_system DROP NOT NULL")
            .await?;
        db.execute_unprepared("ALTER TABLE source_mappings ALTER COLUMN source_system DROP DEFAULT")
            .await?;

        // Change source_key back to INT
        db.execute_unprepared("ALTER TABLE source_mappings ALTER COLUMN source_key TYPE INTEGER USING source_key::INTEGER")
            .await?;

        // Restore old PK
        db.execute_unprepared("ALTER TABLE source_mappings ADD PRIMARY KEY (entity_type, source_key)")
            .await?;

        Ok(())
    }
}

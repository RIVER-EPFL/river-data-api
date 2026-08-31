use sea_orm_migration::prelude::*;

/// Link an annotation back to the audit hold whose resolution minted it.
///
/// A decision taken in the audit queue is invisible from the chart it concerns, which is where the
/// question is usually asked. Minting an annotation puts it on the plot and in the tooltip through
/// the machinery that already exists; the link is what lets a reopen remove exactly the annotation
/// that decision added, without matching on text.
///
/// `ON DELETE SET NULL`: an admin deleting a hold's history keeps the note, orphaned but readable.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE annotations
             ADD COLUMN IF NOT EXISTS audit_hold_id UUID
             REFERENCES replicate_audit_holds(id) ON DELETE SET NULL",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_annotations_audit_hold
             ON annotations (audit_hold_id) WHERE audit_hold_id IS NOT NULL",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_annotations_audit_hold")
            .await?;
        db.execute_unprepared("ALTER TABLE annotations DROP COLUMN IF EXISTS audit_hold_id")
            .await?;
        Ok(())
    }
}

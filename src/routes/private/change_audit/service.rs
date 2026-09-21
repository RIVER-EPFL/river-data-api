//! The trail of one subject, and the append every writer makes.

use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, NotSet, QueryFilter, QueryOrder,
    QuerySelect, Set,
};
use uuid::Uuid;

use super::models::{self, ChangeEntry};
use crate::error::AppResult;

/// The newest edits returned for one subject. A trail is read to answer a question about a
/// specific thing, so the cap is a page size rather than a limit anyone pages past.
const LIMIT: u64 = 100;

/// Up to the 100 newest changes recorded against `subject`, newest first. An unknown subject is an
/// empty list, not a 404: nothing having been done to a thing is an answer.
pub async fn entries_for<C: ConnectionTrait>(db: &C, subject: &str) -> AppResult<Vec<ChangeEntry>> {
    Ok(models::Entity::find()
        .filter(models::Column::Subject.eq(subject))
        .order_by_desc(models::Column::ChangedAt)
        .limit(LIMIT)
        .all(db)
        .await?
        .into_iter()
        .map(ChangeEntry::from)
        .collect())
}

/// The revision a subject stands at: the newest `seq` recorded for it, or `None` for a subject no
/// trigger has seen, which after the backfill is a subject that does not exist.
pub async fn entity_revision<C: ConnectionTrait>(db: &C, subject: &str) -> AppResult<Option<i64>> {
    let revisions = entity_revisions(db, std::slice::from_ref(&subject.to_string())).await?;
    Ok(revisions.get(subject).copied())
}

/// The revision of each of `subjects`, keyed by subject; a subject with no row is absent.
pub async fn entity_revisions<C: ConnectionTrait>(
    db: &C,
    subjects: &[String],
) -> AppResult<std::collections::HashMap<String, i64>> {
    if subjects.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows: Vec<(String, i64)> = models::Entity::find()
        .select_only()
        .column(models::Column::Subject)
        .column_as(models::Column::Seq.max(), "revision")
        .filter(models::Column::Subject.is_in(subjects.to_vec()))
        .group_by(models::Column::Subject)
        .into_tuple()
        .all(db)
        .await?;
    Ok(rows.into_iter().collect())
}

/// Append one change to the trail, in the caller's transaction. Every writer goes through here, so
/// a new one cannot leave out a column the readers expect.
pub async fn record<C: ConnectionTrait>(
    db: &C,
    subject: String,
    change: &str,
    changed_by: Option<String>,
    old_value: Option<serde_json::Value>,
    new_value: Option<serde_json::Value>,
) -> Result<(), sea_orm::DbErr> {
    models::ActiveModel {
        id: Set(Uuid::new_v4()),
        subject: Set(subject),
        change: Set(change.to_string()),
        changed_by: Set(changed_by),
        old_value: Set(old_value),
        new_value: Set(new_value),
        changed_at: Set(chrono::Utc::now().into()),
        seq: NotSet,
    }
    .insert(db)
    .await?;
    Ok(())
}

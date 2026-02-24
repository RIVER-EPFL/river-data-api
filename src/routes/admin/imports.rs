use axum::{extract::{Multipart, State}, Json};
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ConnectionTrait, Set, Statement};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::data_imports;
use crate::error::{AppError, AppResult};

pub async fn upload_csv(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> AppResult<Json<serde_json::Value>> {
    // Extract file from multipart
    let mut file_data: Option<(String, Vec<u8>)> = None;
    while let Some(field) = multipart.next_field().await.map_err(|e| AppError::BadRequest(e.to_string()))? {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            let file_name = field.file_name().unwrap_or("upload.csv").to_string();
            let bytes = field.bytes().await.map_err(|e| AppError::BadRequest(e.to_string()))?;
            file_data = Some((file_name, bytes.to_vec()));
            break;
        }
    }

    let Some((file_name, bytes)) = file_data else {
        return Err(AppError::BadRequest("No file field found in multipart upload".into()));
    };

    // Create data_imports record
    let import_id = Uuid::new_v4();
    let import_record = data_imports::ActiveModel {
        id: Set(import_id),
        source_type: Set("csv_upload".into()),
        file_name: Set(Some(file_name.clone())),
        status: Set("processing".into()),
        started_at: Set(Some(Utc::now())),
        created_at: Set(Some(Utc::now())),
        ..Default::default()
    };
    import_record.insert(&state.db).await?;

    // Fire-and-forget: parse CSV and insert readings in background
    let db = state.db.clone();
    tokio::spawn(async move {
        let result = process_csv_import(&db, import_id, &bytes).await;

        let (status, rows_imported, rows_failed, error_message) = match result {
            Ok((imported, failed)) => ("completed".to_string(), Some(imported), Some(failed), None),
            Err(e) => ("failed".to_string(), Some(0), Some(0), Some(e.to_string())),
        };

        // Update the import record with final status
        if let Err(e) = db
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"UPDATE data_imports SET status = $1, rows_imported = $2, rows_failed = $3,
                  error_message = $4, completed_at = NOW() WHERE id = $5",
                [
                    status.into(),
                    rows_imported.into(),
                    rows_failed.into(),
                    error_message.into(),
                    import_id.into(),
                ],
            ))
            .await
        {
            tracing::error!(import_id = %import_id, error = %e, "Failed to update import record status");
        }
    });

    Ok(Json(serde_json::json!({
        "import_id": import_id,
        "status": "processing",
        "file_name": file_name,
    })))
}

async fn process_csv_import(
    db: &sea_orm::DatabaseConnection,
    _import_id: Uuid,
    bytes: &[u8],
) -> Result<(i32, i32), AppError> {
    let mut rdr = csv::Reader::from_reader(bytes);
    let mut rows_imported = 0i32;
    let mut rows_failed = 0i32;

    // Expected CSV columns: parameter_id, time, raw_value, calibrated_value (optional)
    for result in rdr.records() {
        let record = match result {
            Ok(r) => r,
            Err(_) => {
                rows_failed += 1;
                continue;
            }
        };

        let parameter_id = match record.get(0).and_then(|s| s.parse::<Uuid>().ok()) {
            Some(v) => v,
            None => { rows_failed += 1; continue; }
        };
        let time = match record.get(1).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()) {
            Some(v) => v,
            None => { rows_failed += 1; continue; }
        };
        let raw_value = match record.get(2).and_then(|s| s.parse::<f64>().ok()) {
            Some(v) => v,
            None => { rows_failed += 1; continue; }
        };
        let calibrated_value: Option<f64> = record.get(3).and_then(|s| s.parse::<f64>().ok());

        let res = db
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"INSERT INTO readings (parameter_id, time, raw_value, calibrated_value)
                  VALUES ($1, $2, $3, $4)
                  ON CONFLICT (parameter_id, time) DO UPDATE SET raw_value = $3, calibrated_value = $4",
                [
                    parameter_id.into(),
                    time.into(),
                    raw_value.into(),
                    calibrated_value.into(),
                ],
            ))
            .await;

        match res {
            Ok(_) => rows_imported += 1,
            Err(_) => rows_failed += 1,
        }
    }

    Ok((rows_imported, rows_failed))
}

use sqlx::mysql::{MySqlPool, MySqlPoolOptions};

use crate::config::PortalConfig;
use crate::error::SyncError;

pub async fn create_pool(config: &PortalConfig) -> Result<MySqlPool, SyncError> {
    let url = config.database_url();
    tracing::info!(
        host = %config.db_host,
        port = config.db_port,
        db = %config.db_name,
        "Connecting to portal database"
    );

    let pool = MySqlPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await?;

    // Verify connectivity
    sqlx::query("SELECT 1").execute(&pool).await?;
    tracing::info!("Portal database connection established");

    Ok(pool)
}

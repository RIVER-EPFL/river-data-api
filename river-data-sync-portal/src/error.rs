#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("River Data API error: {0}")]
    Api(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Parse error: {0}")]
    Parse(String),
}

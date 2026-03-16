#[derive(Debug, thiserror::Error)]
pub enum RiverDataClientError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API error: {0}")]
    Api(String),
}

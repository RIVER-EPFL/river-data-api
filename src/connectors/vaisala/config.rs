/// Vaisala-specific configuration extracted from the main Config.
#[derive(Debug, Clone)]
pub struct VaisalaConfig {
    pub base_url: String,
    pub bearer_token: String,
    pub skip_tls_verify: bool,
    pub max_history_days: i64,
}

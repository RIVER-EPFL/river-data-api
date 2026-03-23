use crate::error::SyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalType {
    Cnet,
    Metalp,
    Nomis,
}

impl PortalType {
    pub fn source_system(&self) -> &'static str {
        match self {
            Self::Cnet => "cnet",
            Self::Metalp => "metalp",
            Self::Nomis => "nomis",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PortalConfig {
    pub portal_type: PortalType,
    pub db_host: String,
    pub db_port: u16,
    pub db_user: String,
    pub db_password: String,
    pub db_name: String,
    pub retry_max: u32,
    pub retry_delay_seconds: u64,
}

impl PortalConfig {
    pub fn from_env() -> Result<Self, SyncError> {
        let portal_type = match std::env::var("PORTAL_TYPE")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "cnet" => PortalType::Cnet,
            "metalp" => PortalType::Metalp,
            "nomis" => PortalType::Nomis,
            other => {
                return Err(SyncError::Config(format!(
                    "Unknown PORTAL_TYPE: '{other}'. Expected: cnet, metalp, nomis"
                )));
            }
        };

        Ok(Self {
            portal_type,
            db_host: require_env("PORTAL_DB_HOST")?,
            db_port: env_u16("PORTAL_DB_PORT", 3306),
            db_user: require_env("PORTAL_DB_USER")?,
            db_password: require_env("PORTAL_DB_PASSWORD")?,
            db_name: require_env("PORTAL_DB_NAME")?,
            retry_max: env_u32("RETRY_MAX", 3),
            retry_delay_seconds: env_u64("RETRY_DELAY_SECONDS", 60),
        })
    }

    pub fn database_url(&self) -> String {
        format!(
            "mysql://{}:{}@{}:{}/{}",
            self.db_user, self.db_password, self.db_host, self.db_port, self.db_name
        )
    }
}

fn require_env(key: &str) -> Result<String, SyncError> {
    std::env::var(key).map_err(|_| SyncError::Config(format!("Missing required env var: {key}")))
}

fn env_u16(key: &str, default: u16) -> u16 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

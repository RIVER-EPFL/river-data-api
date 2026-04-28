#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Role {
    Administrator,
    User,
    Unknown(String),
}

impl axum_keycloak_auth::role::Role for Role {}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::Administrator => f.write_str("riverdata-admin"),
            Role::User => f.write_str("riverdata-user"),
            Role::Unknown(unknown) => f.write_fmt(format_args!("Unknown role: {unknown}")),
        }
    }
}

impl From<String> for Role {
    fn from(value: String) -> Self {
        match value.as_ref() {
            "riverdata-admin" => Role::Administrator,
            "riverdata-user" => Role::User,
            _ => Role::Unknown(value),
        }
    }
}

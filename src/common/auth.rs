#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Role {
    Administrator,
    User,
    Unknown(String),
}

impl Role {
    /// Whether this realm role grants access to the application at all. The RIVER realm is
    /// EPFL-federated and auto-assigns `default-roles-river` to every EPFL login, so
    /// authentication alone proves nothing — only the explicit `riverdata-*` roles admit a user.
    /// Unrelated realm roles (`admin`, `offline_access`, `uma_authorization`, …) land in
    /// `Unknown` and are ignored.
    #[must_use]
    pub fn grants_access(&self) -> bool {
        matches!(self, Role::Administrator | Role::User)
    }
}

/// A single authorization capability. Both Keycloak roles and API-token permissions resolve into a
/// set of these (see `AuthContext::allows`), so every route gate is expressed once as "requires
/// capability X" instead of each middleware re-deriving the Keycloak-vs-token policy by hand. This
/// is the seam the coming Keycloak-role RBAC extends: new roles map to capability sets in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    ReadMetadata,
    ReadData,
    WriteMetadata,
    WriteData,
    /// Privileged management (mint/list/revoke tokens, sync credentials, user admin). Granted only
    /// to the Keycloak Administrator role — never to an API token, by design.
    Admin,
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Capability::ReadMetadata => "read_metadata",
            Capability::ReadData => "read_data",
            Capability::WriteMetadata => "write_metadata",
            Capability::WriteData => "write_data",
            Capability::Admin => "admin",
        })
    }
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

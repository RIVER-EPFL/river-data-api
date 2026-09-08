//! Which services a run covers, declared by `TEST_PROFILE` rather than detected.
//!
//! Selection, not detection: a gated test whose service the profile covers fails when that service
//! is unreachable, naming the service and the profile, and a test the profile excludes does not
//! run. Probing survives only as the failure message's evidence.

/// A service a test binary needs but does not start.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Service {
    Keycloak,
    ToolsRunner,
}

/// The set of services a run declares it covers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Profile {
    All,
    NoKeycloak,
    NoR,
    UnitOnly,
}

pub const PROFILE_VAR: &str = "TEST_PROFILE";

impl Service {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Service::Keycloak => "Keycloak",
            Service::ToolsRunner => "the analytical tool runner",
        }
    }

    fn url(self) -> String {
        match self {
            Service::Keycloak => super::keycloak::keycloak_base_url(),
            Service::ToolsRunner => super::tools_runner::runner_url(),
        }
    }

    async fn reachable(self) -> bool {
        match self {
            Service::Keycloak => super::keycloak::keycloak_reachable().await,
            Service::ToolsRunner => super::tools_runner::reachable().await,
        }
    }

    /// Gate a test on this service: true to proceed, false when the profile excludes it.
    ///
    /// # Panics
    /// When the profile covers the service and the service is unreachable.
    pub async fn require(self, test_name: &str) -> bool {
        let profile = selected();
        if !profile.covers(self) {
            return false;
        }
        assert!(
            self.reachable().await,
            "test profile `{}` covers {}, but it is unreachable at {}, so {test_name} cannot run",
            profile.name(),
            self.name(),
            self.url()
        );
        true
    }
}

impl Profile {
    /// # Errors
    /// When the name is not one of the four profiles.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "all" => Ok(Profile::All),
            "no-keycloak" => Ok(Profile::NoKeycloak),
            "no-r" => Ok(Profile::NoR),
            "unit-only" => Ok(Profile::UnitOnly),
            other => Err(format!(
                "unknown {PROFILE_VAR} `{other}`, expected all, no-keycloak, no-r or unit-only"
            )),
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Profile::All => "all",
            Profile::NoKeycloak => "no-keycloak",
            Profile::NoR => "no-r",
            Profile::UnitOnly => "unit-only",
        }
    }

    #[must_use]
    pub fn covers(self, service: Service) -> bool {
        match self {
            Profile::All => true,
            Profile::NoKeycloak => service != Service::Keycloak,
            Profile::NoR => service != Service::ToolsRunner,
            Profile::UnitOnly => false,
        }
    }
}

/// The profile this run declared. Unset is `all`, so a run that says nothing is held to everything.
///
/// # Panics
/// When `TEST_PROFILE` names a profile that does not exist.
#[must_use]
pub fn selected() -> Profile {
    dotenvy::dotenv().ok();
    match std::env::var(PROFILE_VAR) {
        Ok(raw) => Profile::parse(&raw).unwrap_or_else(|e| panic!("{e}")),
        Err(_) => Profile::All,
    }
}

#[cfg(test)]
mod tests {
    use super::{Profile, Service};

    #[test]
    fn test_profile_parse_rejects_unknown() {
        assert_eq!(Profile::parse("all"), Ok(Profile::All));
        assert_eq!(Profile::parse(" no-keycloak "), Ok(Profile::NoKeycloak));
        let err = Profile::parse("keycloak").unwrap_err();
        assert!(err.contains("unknown TEST_PROFILE `keycloak`"), "{err}");
    }

    #[test]
    fn test_profile_covers_by_service() {
        assert!(Profile::All.covers(Service::Keycloak));
        assert!(Profile::All.covers(Service::ToolsRunner));
        assert!(!Profile::NoKeycloak.covers(Service::Keycloak));
        assert!(Profile::NoKeycloak.covers(Service::ToolsRunner));
        assert!(Profile::NoR.covers(Service::Keycloak));
        assert!(!Profile::NoR.covers(Service::ToolsRunner));
        assert!(!Profile::UnitOnly.covers(Service::Keycloak));
        assert!(!Profile::UnitOnly.covers(Service::ToolsRunner));
    }
}

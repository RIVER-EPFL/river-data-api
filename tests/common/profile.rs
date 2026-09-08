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

    /// The profile that covers everything except this service, which the failure message offers.
    fn excluded_by(self) -> Profile {
        match self {
            Service::Keycloak => Profile::NoKeycloak,
            Service::ToolsRunner => Profile::NoR,
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
            "test profile `{}` covers {}, but it is unreachable at {}, so {test_name} cannot run. \
             Run it where the service is, or exclude it: {PROFILE_VAR}={}",
            profile.name(),
            self.name(),
            self.url(),
            self.excluded_by().name()
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

/// What a checkout covers when the run declares nothing: everything but the R tool runner.
///
/// The runner is published nowhere. In compose it is on an internal network with no `ports:`, and
/// on k8s it is a sidecar bound to the pod's loopback, so a run from the host cannot reach it
/// however healthy the container is. Holding an undeclared run to it makes the `tools` theme red
/// for a reason that is not the caller's. Keycloak is the other way round: compose publishes 8180,
/// which is what `keycloak_base_url` defaults to. The runs that do cover the runner say so:
/// `TEST_PROFILE=all` in CI and in the compose watcher, which run beside it.
pub const HOST_DEFAULT: Profile = Profile::NoR;

/// The profile this run declared, or [`HOST_DEFAULT`] when it declared none.
///
/// # Panics
/// When `TEST_PROFILE` names a profile that does not exist.
#[must_use]
pub fn selected() -> Profile {
    dotenvy::dotenv().ok();
    match std::env::var(PROFILE_VAR) {
        Ok(raw) => Profile::parse(&raw).unwrap_or_else(|e| panic!("{e}")),
        Err(_) => HOST_DEFAULT,
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

    /// The default is the one a checkout can actually meet: the runner is published nowhere, so an
    /// undeclared run covering it would be red on every host.
    #[test]
    fn test_host_default_covers_keycloak_and_not_the_runner() {
        assert!(super::HOST_DEFAULT.covers(Service::Keycloak));
        assert!(!super::HOST_DEFAULT.covers(Service::ToolsRunner));
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

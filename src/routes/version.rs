use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

/// Build/version metadata for the running binary. All values are baked in at compile time:
/// `version` from Cargo, `commit`/`built_at` from build-args CI sets (tag on prod, short SHA on
/// dev). The pod sources nothing at runtime, so this can never drift from the image it reports.
#[derive(Serialize, ToSchema)]
pub struct VersionInfo {
    pub name: &'static str,
    pub version: &'static str,
    pub commit: &'static str,
    pub built_at: &'static str,
    /// The public serving contract this build implements, what a project's docs advertise when
    /// it pins no `public_api_version` of its own.
    pub public_api_contract: &'static str,
}

const COMMIT: &str = match option_env!("BUILD_VERSION") {
    Some(v) => v,
    None => "dev",
};
const BUILT_AT: &str = match option_env!("BUILD_TIME") {
    Some(v) => v,
    None => "unknown",
};

/// Returns the API's build/version metadata. Requires `read_metadata` (authenticated only, build
/// details are not exposed to anonymous callers).
#[utoipa::path(
    get,
    path = "/api/version",
    responses((status = 200, description = "Build/version metadata", body = VersionInfo)),
    tag = "health"
)]
pub async fn get_version() -> Json<VersionInfo> {
    Json(VersionInfo {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        commit: COMMIT,
        built_at: BUILT_AT,
        public_api_contract: crate::routes::public::service::SERVING_CONTRACT_VERSION,
    })
}

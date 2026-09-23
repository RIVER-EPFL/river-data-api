pub mod common;
pub mod config;
pub mod error;
pub mod restore;
pub mod routes;

/// The crate `cargo test` was started in, read when the test runs rather than when it was built,
/// so a binary compiled from another checkout into a shared target still scans this tree.
#[cfg(test)]
pub(crate) fn test_crate_root() -> std::path::PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("cargo sets CARGO_MANIFEST_DIR for a test run")
        .into()
}

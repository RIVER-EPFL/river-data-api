//! Analytical tools: DB-stored, versioned R scripts executed by the OpenCPU runner.
//!
//! `service` loads active versions and proxies calculation; `views` is the admin authoring
//! surface (versions, validation, activation). The portal calculation functions themselves live
//! inside the seeded scripts, verbatim.

pub mod flows;
pub mod models;
pub mod service;
pub mod staged;
pub mod views;

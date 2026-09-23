//! Analytical tools: DB-stored, versioned R scripts executed by the OpenCPU runner.
//!
//! `service` loads active versions and proxies calculation; `views` is the admin authoring
//! surface (versions, validation, activation). A database starts blank of tools: every script is
//! authored through `/tool_scripts`.

pub mod flows;
pub mod models;
pub mod service;
pub mod staged;
pub mod views;

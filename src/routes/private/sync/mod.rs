//! Sync: the control plane a sync service calls, the operator surface a human drives, and the
//! pairing-plan and replicate-audit machinery between them.

pub mod flows;
pub mod hold_model;
pub mod models;
pub mod service;
pub mod views;

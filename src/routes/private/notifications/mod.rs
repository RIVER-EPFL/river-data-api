//! Email + Telegram notifications layered on the alarm pipeline.
//!
//! The dispatcher consumes `AlarmStateChanged` broadcasts (and polls the outbox columns as a
//! backstop), renders messages, and fans them out to the enabled channels. Telegram is restricted
//! to linked identities whose Keycloak role is resolved live (`authz`), never from a cached value.

pub mod identities_model;
pub mod log_model;
pub mod mutes_model;

pub use identities_model::TelegramIdentity;
pub use log_model::NotificationLog;
pub use mutes_model::NotificationMute;

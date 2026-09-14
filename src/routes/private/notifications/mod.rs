//! Notification delivery layered on the alarm pipeline.
//!
//! The dispatcher consumes `AlarmStateChanged` broadcasts (and polls the outbox columns as a
//! backstop), renders messages, and fans them out to the enabled Web Push channel.

pub mod flows;
pub mod models;
pub mod service;
pub mod views;

pub use models::log::NotificationLog;
pub use models::mutes::NotificationMute;
pub use models::state::NotificationState;
pub use models::subscriber::NotificationSubscriber;

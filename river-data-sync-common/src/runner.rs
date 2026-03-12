use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, watch};
use uuid::Uuid;

use crate::client::{ControlPlaneClient, ControlPlaneError};
use crate::commands;
use crate::models::{PendingCommand, RunnerConfig, SyncResult};

#[async_trait::async_trait]
pub trait SyncService: Send + Sync + 'static {
    fn service_type(&self) -> &str;

    async fn sync(&self, full: bool) -> Result<SyncResult, Box<dyn std::error::Error + Send + Sync>>;

    async fn handle_command(
        &self,
        command: &str,
        _payload: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(format!("Unknown command: {command}").into())
    }

    fn update_token(&self, token: &str);
}

pub struct SyncServiceRunner<S: SyncService> {
    service: Arc<S>,
    config: RunnerConfig,
}

impl<S: SyncService> SyncServiceRunner<S> {
    pub fn new(service: S, config: RunnerConfig) -> Self {
        Self {
            service: Arc::new(service),
            config,
        }
    }

    pub async fn run(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut client = ControlPlaneClient::new(&self.config.api_base_url);

        // Enroll
        tracing::info!(
            client_id = %self.config.client_id,
            instance_id = %self.config.instance_id,
            "Enrolling with control plane"
        );

        let enroll_resp = loop {
            match client
                .enroll(
                    &self.config.client_id,
                    &self.config.client_secret,
                    &self.config.instance_id,
                )
                .await
            {
                Ok(resp) => break resp,
                Err(ControlPlaneError::CredentialsRevoked) => {
                    tracing::error!("Credentials revoked or invalid — cannot enroll. Exiting.");
                    return Err("Credentials revoked".into());
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Enrollment failed, retrying in 10s");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        };

        let service_id = enroll_resp.service_id;
        self.service.update_token(&enroll_resp.session_token);
        tracing::info!(%service_id, "Enrolled successfully");

        // Shared state
        let (pause_tx, pause_rx) = watch::channel(false);
        let sync_notify = Arc::new(Notify::new());
        let full_sync_notify = Arc::new(Notify::new());

        // Spawn heartbeat loop
        let hb_service = self.service.clone();
        let hb_config = self.config.clone();
        let hb_sync_notify = sync_notify.clone();
        let hb_full_sync_notify = full_sync_notify.clone();
        let hb_pause_tx = pause_tx.clone();

        let heartbeat_handle = tokio::spawn(async move {
            Self::heartbeat_loop(
                client,
                service_id,
                hb_config,
                hb_service,
                hb_sync_notify,
                hb_full_sync_notify,
                hb_pause_tx,
            )
            .await;
        });

        // Spawn sync loop
        let sync_service = self.service.clone();
        let sync_interval = self.config.sync_interval_secs;
        let sync_handle = tokio::spawn(async move {
            Self::sync_loop(
                sync_service,
                sync_interval,
                pause_rx,
                sync_notify,
                full_sync_notify,
            )
            .await;
        });

        // Wait for shutdown signal
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received shutdown signal");
            }
            _ = heartbeat_handle => {
                tracing::error!("Heartbeat loop exited unexpectedly");
            }
            _ = sync_handle => {
                tracing::error!("Sync loop exited unexpectedly");
            }
        }

        Ok(())
    }

    async fn heartbeat_loop(
        mut client: ControlPlaneClient,
        service_id: Uuid,
        config: RunnerConfig,
        service: Arc<S>,
        sync_notify: Arc<Notify>,
        full_sync_notify: Arc<Notify>,
        pause_tx: watch::Sender<bool>,
    ) {
        let mut interval =
            tokio::time::interval(Duration::from_secs(config.heartbeat_interval_secs));
        interval.tick().await; // skip first immediate tick

        loop {
            interval.tick().await;

            let status = if *pause_tx.borrow() { "paused" } else { "running" };

            match client
                .heartbeat(service_id, &config.client_secret, status, None)
                .await
            {
                Ok(resp) => {
                    service.update_token(&resp.session_token);

                    for cmd in resp.pending_commands {
                        Self::handle_command(
                            &client,
                            &service,
                            cmd,
                            &sync_notify,
                            &full_sync_notify,
                            &pause_tx,
                        )
                        .await;
                    }
                }
                Err(ControlPlaneError::CredentialsRevoked) => {
                    tracing::error!("Credentials revoked — attempting re-enrollment");
                    match client
                        .enroll(&config.client_id, &config.client_secret, &config.instance_id)
                        .await
                    {
                        Ok(resp) => {
                            service.update_token(&resp.session_token);
                            tracing::info!("Re-enrolled successfully");
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Re-enrollment failed");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Heartbeat failed");
                }
            }
        }
    }

    async fn handle_command(
        client: &ControlPlaneClient,
        service: &Arc<S>,
        cmd: PendingCommand,
        sync_notify: &Arc<Notify>,
        full_sync_notify: &Arc<Notify>,
        pause_tx: &watch::Sender<bool>,
    ) {
        tracing::info!(command = %cmd.command, id = %cmd.id, "Received command");

        // Acknowledge immediately
        let _ = client
            .update_command(cmd.id, "acknowledged", None)
            .await;

        match cmd.command.as_str() {
            commands::TRIGGER_SYNC => {
                sync_notify.notify_one();
                let _ = client
                    .update_command(
                        cmd.id,
                        "completed",
                        Some(serde_json::json!({"triggered": true})),
                    )
                    .await;
            }
            commands::TRIGGER_FULL_SYNC => {
                full_sync_notify.notify_one();
                let _ = client
                    .update_command(
                        cmd.id,
                        "completed",
                        Some(serde_json::json!({"triggered": true, "full": true})),
                    )
                    .await;
            }
            commands::PAUSE => {
                let _ = pause_tx.send(true);
                let _ = client
                    .update_command(
                        cmd.id,
                        "completed",
                        Some(serde_json::json!({"paused": true})),
                    )
                    .await;
            }
            commands::RESUME => {
                let _ = pause_tx.send(false);
                let _ = client
                    .update_command(
                        cmd.id,
                        "completed",
                        Some(serde_json::json!({"resumed": true})),
                    )
                    .await;
            }
            other => {
                // Delegate to service-specific handler
                match service.handle_command(other, cmd.payload).await {
                    Ok(result) => {
                        let _ = client
                            .update_command(cmd.id, "completed", Some(result))
                            .await;
                    }
                    Err(e) => {
                        let _ = client
                            .update_command(
                                cmd.id,
                                "failed",
                                Some(serde_json::json!({"error": e.to_string()})),
                            )
                            .await;
                    }
                }
            }
        }
    }

    async fn sync_loop(
        service: Arc<S>,
        sync_interval_secs: u64,
        pause_rx: watch::Receiver<bool>,
        sync_notify: Arc<Notify>,
        full_sync_notify: Arc<Notify>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(sync_interval_secs));
        interval.tick().await; // skip first immediate tick

        loop {
            let full = tokio::select! {
                _ = interval.tick() => false,
                _ = sync_notify.notified() => false,
                _ = full_sync_notify.notified() => true,
            };

            // Check if paused
            if *pause_rx.borrow() && !full {
                tracing::debug!("Sync paused, skipping scheduled sync");
                continue;
            }

            tracing::info!(full, "Starting sync cycle");
            let start = Instant::now();

            match service.sync(full).await {
                Ok(result) => {
                    let elapsed = start.elapsed();
                    tracing::info!(
                        readings = result.readings_synced,
                        status_events = result.status_events_synced,
                        full = result.full_sync,
                        duration_ms = elapsed.as_millis() as u64,
                        "Sync completed"
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, "Sync failed");
                }
            }
        }
    }
}

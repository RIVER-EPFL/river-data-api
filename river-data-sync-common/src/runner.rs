use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::client::{ControlPlaneClient, ControlPlaneError};
use crate::commands;
use crate::models::{PendingCommand, RunnerConfig, SyncResult, SyncTrigger};
use crate::river_data_client::RiverDataClient;

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

    fn river_data_client(&self) -> Option<&RiverDataClient> {
        None
    }
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
        let mut client = ControlPlaneClient::new(&self.config.api_base_url)?;

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
        let (sync_tx, sync_rx) = mpsc::channel::<SyncTrigger>(16);
        let (current_op_tx, current_op_rx) = watch::channel::<Option<String>>(None);

        // Trigger a full sync immediately after enrollment so the service
        // doesn't wait for the first interval tick (which could be hours).
        let _ = sync_tx.send(SyncTrigger::Scheduled).await;
        tracing::info!("Queued initial sync after enrollment");

        // Spawn heartbeat loop
        let hb_service = self.service.clone();
        let hb_config = self.config.clone();
        let hb_sync_tx = sync_tx.clone();
        let hb_pause_tx = pause_tx.clone();

        let heartbeat_handle = tokio::spawn(async move {
            Self::heartbeat_loop(
                client,
                service_id,
                hb_config,
                hb_service,
                hb_sync_tx,
                hb_pause_tx,
                current_op_rx,
            )
            .await;
        });

        // Spawn sync loop
        let sync_service = self.service.clone();
        let sync_interval = self.config.sync_interval_secs;
        let sync_handle = tokio::spawn(async move {
            Self::sync_loop(
                sync_service,
                service_id,
                sync_interval,
                pause_rx,
                sync_rx,
                current_op_tx,
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
        sync_tx: mpsc::Sender<SyncTrigger>,
        pause_tx: watch::Sender<bool>,
        current_op_rx: watch::Receiver<Option<String>>,
    ) {
        let mut interval =
            tokio::time::interval(Duration::from_secs(config.heartbeat_interval_secs));
        interval.tick().await; // skip first immediate tick

        loop {
            interval.tick().await;

            let is_paused = *pause_tx.borrow();
            let current_op = current_op_rx.borrow().clone();

            let status = if is_paused {
                "paused"
            } else if current_op.is_some() {
                "syncing"
            } else {
                "idle"
            };

            match client
                .heartbeat(service_id, &config.client_secret, status, current_op.as_deref())
                .await
            {
                Ok(resp) => {
                    service.update_token(&resp.session_token);

                    for cmd in resp.pending_commands {
                        Self::handle_command(
                            &client,
                            &service,
                            cmd,
                            &sync_tx,
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
        sync_tx: &mpsc::Sender<SyncTrigger>,
        pause_tx: &watch::Sender<bool>,
    ) {
        tracing::info!(command = %cmd.command, id = %cmd.id, "Received command");

        // Acknowledge immediately
        let _ = client
            .update_command(cmd.id, "acknowledged", None)
            .await;

        match cmd.command.as_str() {
            commands::TRIGGER_SYNC => {
                let _ = sync_tx.send(SyncTrigger::Command { id: cmd.id, full: false }).await;
            }
            commands::TRIGGER_FULL_SYNC => {
                let _ = sync_tx.send(SyncTrigger::Command { id: cmd.id, full: true }).await;
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
        service_id: Uuid,
        sync_interval_secs: u64,
        pause_rx: watch::Receiver<bool>,
        mut sync_rx: mpsc::Receiver<SyncTrigger>,
        current_op_tx: watch::Sender<Option<String>>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(sync_interval_secs));
        interval.tick().await; // skip first immediate tick

        loop {
            let trigger = tokio::select! {
                _ = interval.tick() => SyncTrigger::Scheduled,
                Some(t) = sync_rx.recv() => t,
            };

            let (full, command_id) = match &trigger {
                SyncTrigger::Scheduled => (false, None),
                SyncTrigger::Command { id, full } => (*full, Some(*id)),
            };

            // Check if paused (allow triggered syncs through)
            if *pause_rx.borrow() && command_id.is_none() {
                tracing::debug!("Sync paused, skipping scheduled sync");
                continue;
            }

            tracing::info!(full, "Starting sync cycle");
            let start = Instant::now();

            // Determine event type for logging
            let event_type = match &trigger {
                SyncTrigger::Command { full: true, .. } => "full_sync",
                SyncTrigger::Command { full: false, .. } => "triggered",
                SyncTrigger::Scheduled => "scheduled",
            };

            // Signal current operation to heartbeat loop
            let op_label = if full { "Full Sync" } else { "Syncing" };
            let _ = current_op_tx.send(Some(op_label.to_string()));

            // Create sync event (running)
            let event_id = if let Some(api) = service.river_data_client() {
                match api.create_sync_event(&serde_json::json!({
                    "service_id": service_id,
                    "command_id": command_id,
                    "event_type": event_type,
                    "status": "running",
                })).await {
                    Ok(ev) => ev.get("id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok()),
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to create sync event");
                        None
                    }
                }
            } else {
                None
            };

            let result = service.sync(full).await;

            // Clear current operation
            let _ = current_op_tx.send(None);

            // Update sync event with result
            if let (Some(eid), Some(api)) = (event_id, service.river_data_client()) {
                match &result {
                    Ok(r) => {
                        let status = if r.errors.is_empty() { "completed" } else { "partial" };
                        let _ = api.update_sync_event(eid, &serde_json::json!({
                            "status": status,
                            "readings_synced": r.readings_synced,
                            "status_events_synced": r.status_events_synced,
                            "errors": r.errors,
                            "log": r.log,
                            "duration_ms": r.duration_ms,
                        })).await;
                    }
                    Err(e) => {
                        let duration_ms = start.elapsed().as_millis() as u64;
                        let _ = api.update_sync_event(eid, &serde_json::json!({
                            "status": "failed",
                            "errors": [e.to_string()],
                            "duration_ms": duration_ms,
                        })).await;
                    }
                }
            }

            // Report command result back to control plane
            if let (Some(cmd_id), Some(api)) = (command_id, service.river_data_client()) {
                let (cmd_status, result_json) = match &result {
                    Ok(r) => {
                        ("completed", serde_json::json!({
                            "readings_synced": r.readings_synced,
                            "status_events_synced": r.status_events_synced,
                            "errors": r.errors,
                            "duration_ms": r.duration_ms,
                        }))
                    }
                    Err(e) => {
                        let duration_ms = start.elapsed().as_millis() as u64;
                        ("failed", serde_json::json!({
                            "error": e.to_string(),
                            "duration_ms": duration_ms,
                        }))
                    }
                };
                if let Err(e) = api.update_command(cmd_id, cmd_status, Some(result_json)).await {
                    tracing::warn!(error = %e, "Failed to update command status");
                }
            }

            match &result {
                Ok(r) => {
                    tracing::info!(
                        readings = r.readings_synced,
                        status_events = r.status_events_synced,
                        full = r.full_sync,
                        duration_ms = r.duration_ms,
                        errors = r.errors.len(),
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

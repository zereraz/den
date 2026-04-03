use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::watch;

use crate::config::DenConfig;
use crate::docker::DockerManager;
use crate::sandbox::{Sandbox, SandboxState};

pub struct Reaper {
    sandboxes: Arc<DashMap<String, Sandbox>>,
    docker: Arc<DockerManager>,
    config: Arc<DenConfig>,
}

impl Reaper {
    pub fn new(
        sandboxes: Arc<DashMap<String, Sandbox>>,
        docker: Arc<DockerManager>,
        config: Arc<DenConfig>,
    ) -> Self {
        Self {
            sandboxes,
            docker,
            config,
        }
    }

    /// Run the reaper loop. Returns when shutdown signal is received.
    pub async fn run(&self, mut shutdown_rx: watch::Receiver<bool>) {
        let interval = std::time::Duration::from_secs(self.config.reaper.interval_secs);

        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    self.sweep().await;
                }
                _ = shutdown_rx.changed() => {
                    tracing::info!("reaper shutting down");
                    return;
                }
            }
        }
    }

    async fn sweep(&self) {
        let idle_timeout = self.config.reaper.idle_timeout_secs;
        let volume_ttl = self.config.reaper.volume_ttl_secs;

        // Phase 1: stop running containers that are expired/idle/crashed
        let mut to_stop = Vec::new();

        for entry in self.sandboxes.iter() {
            let sandbox = entry.value();

            if sandbox.state == SandboxState::Running {
                if sandbox.is_expired() {
                    tracing::info!(sandbox = %sandbox.id, "expired, stopping");
                    to_stop.push((sandbox.id.clone(), sandbox.container_id.clone(), SandboxState::Timeout));
                    continue;
                }

                if sandbox.is_idle(idle_timeout) {
                    tracing::info!(sandbox = %sandbox.id, "idle, stopping");
                    to_stop.push((sandbox.id.clone(), sandbox.container_id.clone(), SandboxState::Timeout));
                    continue;
                }

                match self.docker.container_is_running(&sandbox.container_id).await {
                    Ok(false) => {
                        tracing::warn!(sandbox = %sandbox.id, "container exited unexpectedly, marking defunct");
                        to_stop.push((sandbox.id.clone(), sandbox.container_id.clone(), SandboxState::Defunct));
                    }
                    Err(e) => {
                        tracing::warn!(sandbox = %sandbox.id, error = %e, "inspect failed");
                    }
                    _ => {}
                }
            }
        }

        // Stop containers but keep sandbox + volume in DashMap
        for (sandbox_id, container_id, target_state) in to_stop {
            if let Some(mut entry) = self.sandboxes.get_mut(&sandbox_id) {
                let _ = entry.value_mut().transition(target_state);
            }

            if target_state == SandboxState::Timeout {
                let _ = self.docker.stop_container(&container_id, 5).await;
            }
            let _ = self.docker.remove_container(&container_id).await;

            tracing::info!(sandbox = %sandbox_id, "container reaped (volume kept)");
        }

        // Phase 2: full cleanup of terminal/defunct sandboxes past volume TTL
        let mut to_destroy = Vec::new();

        for entry in self.sandboxes.iter() {
            let sandbox = entry.value();

            if sandbox.state.is_terminal() || sandbox.state == SandboxState::Defunct {
                let age = chrono::Utc::now().signed_duration_since(sandbox.last_activity);
                if age.num_seconds() as u64 >= volume_ttl {
                    to_destroy.push((
                        sandbox.id.clone(),
                        sandbox.container_id.clone(),
                        sandbox.volume_name.clone(),
                    ));
                }
            }
        }

        for (sandbox_id, container_id, vol_name) in to_destroy {
            // Re-check state under lock to avoid racing with resume()
            let still_terminal = self
                .sandboxes
                .get(&sandbox_id)
                .map(|e| e.value().state.is_terminal())
                .unwrap_or(false);

            if !still_terminal {
                tracing::info!(sandbox = %sandbox_id, "skipping destroy, sandbox was resumed");
                continue;
            }

            let _ = self.docker.remove_container(&container_id).await;
            let _ = self.docker.remove_volume(&vol_name).await;
            self.sandboxes.remove(&sandbox_id);

            tracing::info!(sandbox = %sandbox_id, volume = %vol_name, "fully reaped (volume removed)");
        }
    }
}

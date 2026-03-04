use std::sync::Arc;

use serde::Serialize;

use crate::config::DenConfig;
use crate::docker::DockerManager;
use crate::error::DenError;

pub struct Scheduler {
    docker: Arc<DockerManager>,
    config: Arc<DenConfig>,
}

#[derive(Debug, Serialize)]
pub struct ResourceSnapshot {
    pub total_memory_bytes: u64,
    pub used_memory_bytes: u64,
    pub cpus: u64,
    pub containers_running: u64,
    pub containers_total: u64,
}

impl Scheduler {
    pub fn new(docker: Arc<DockerManager>, config: Arc<DenConfig>) -> Self {
        Self { docker, config }
    }

    /// Check if we have enough resources to schedule a container for the given tier.
    pub async fn can_schedule(&self, tier: &str) -> Result<bool, DenError> {
        let tier_config = self
            .config
            .tiers
            .get(tier)
            .ok_or_else(|| DenError::TierNotFound { tier: tier.into() })?;

        let info = self.docker.client().info().await?;

        let total_mem = info.mem_total.unwrap_or(0) as u64;
        let ncpu = info.ncpu.unwrap_or(0) as u64;
        let running = info.containers_running.unwrap_or(0) as u64;

        let needed_mem = tier_config.memory_mb * 1024 * 1024;
        // Rough heuristic: reserve 20% of system memory for the host
        let available_mem = total_mem.saturating_sub(total_mem / 5);
        let estimated_used = running * needed_mem; // rough approximation

        if estimated_used + needed_mem > available_mem {
            return Ok(false);
        }

        // Check CPU: ensure tier cpus don't exceed total
        if tier_config.cpus > ncpu as f64 {
            return Ok(false);
        }

        Ok(true)
    }

    /// Get a snapshot of system resources from Docker.
    pub async fn resource_snapshot(&self) -> Result<ResourceSnapshot, DenError> {
        let info = self.docker.client().info().await?;

        Ok(ResourceSnapshot {
            total_memory_bytes: info.mem_total.unwrap_or(0) as u64,
            used_memory_bytes: 0, // Docker info doesn't give used; would need cgroup stats
            cpus: info.ncpu.unwrap_or(0) as u64,
            containers_running: info.containers_running.unwrap_or(0) as u64,
            containers_total: info.containers.unwrap_or(0) as u64,
        })
    }
}

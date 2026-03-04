use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;

use crate::config::DenConfig;
use crate::docker::DockerManager;
use crate::error::DenError;
use crate::sandbox::{Sandbox, SandboxState};
use crate::scheduler::Scheduler;

/// A pre-created container + volume pair sitting in the pool, ready to be claimed.
struct PoolEntry {
    container_id: String,
    volume_name: String,
}

/// Per-tier queue of pre-created entries.
struct TierPool {
    available: Mutex<Vec<PoolEntry>>,
    target_size: usize,
}

pub struct Pool {
    tiers: HashMap<String, TierPool>,
    docker: Arc<DockerManager>,
    config: Arc<DenConfig>,
    pub sandboxes: Arc<DashMap<String, Sandbox>>,
    scheduler: Arc<Scheduler>,
}

impl Pool {
    pub fn new(
        docker: Arc<DockerManager>,
        config: Arc<DenConfig>,
        sandboxes: Arc<DashMap<String, Sandbox>>,
        scheduler: Arc<Scheduler>,
    ) -> Self {
        let mut tiers = HashMap::new();
        for (name, tier) in &config.tiers {
            tiers.insert(
                name.clone(),
                TierPool {
                    available: Mutex::new(Vec::with_capacity(tier.pool_size)),
                    target_size: tier.pool_size,
                },
            );
        }
        Self {
            tiers,
            docker,
            config,
            sandboxes,
            scheduler,
        }
    }

    /// Pre-create containers + volumes for all tiers. Called once at startup.
    pub async fn warm_up(&self) -> Result<(), DenError> {
        let mut handles: Vec<(
            String,
            tokio::task::JoinHandle<Result<(String, String), DenError>>,
        )> = Vec::new();

        for (tier_name, tier_config) in &self.config.tiers {
            for _ in 0..tier_config.pool_size {
                let docker = self.docker.clone();
                let tier = tier_config.clone();
                let name = container_name(tier_name);
                let vol = volume_name();
                let handle = tokio::spawn(async move {
                    docker.create_volume(&vol).await?;
                    let cid = docker.create_container(&tier, &name, &vol).await?;
                    Ok((cid, vol))
                });
                handles.push((tier_name.clone(), handle));
            }
        }

        for (tier_name, handle) in handles {
            let (container_id, vol_name) = handle
                .await
                .map_err(|e| DenError::Config(format!("spawn: {e}")))??;

            if let Some(tier_pool) = self.tiers.get(&tier_name) {
                tier_pool.available.lock().await.push(PoolEntry {
                    container_id,
                    volume_name: vol_name,
                });
            }
        }

        for (tier_name, tier_pool) in &self.tiers {
            let count = tier_pool.available.lock().await.len();
            tracing::info!(tier = %tier_name, count, "pool warmed");
        }

        Ok(())
    }

    /// Claim a sandbox from the pool. Starts the container and returns a Running sandbox.
    pub async fn claim(
        &self,
        tier_name: &str,
        metadata: HashMap<String, String>,
    ) -> Result<Sandbox, DenError> {
        let tier_config = self
            .config
            .tiers
            .get(tier_name)
            .ok_or_else(|| DenError::TierNotFound {
                tier: tier_name.into(),
            })?;

        // Check host has enough resources before claiming
        if !self.scheduler.can_schedule(tier_name).await? {
            return Err(DenError::InsufficientResources {
                reason: format!("not enough resources for tier {tier_name}"),
            });
        }

        let tier_pool = self
            .tiers
            .get(tier_name)
            .ok_or_else(|| DenError::TierNotFound {
                tier: tier_name.into(),
            })?;

        let entry = {
            let mut available = tier_pool.available.lock().await;
            available.pop().ok_or_else(|| DenError::PoolExhausted {
                tier: tier_name.into(),
            })?
        };

        // Start the pre-created container
        self.docker.start_container(&entry.container_id).await?;

        let sid = sandbox_id(tier_name);
        let mut sandbox = Sandbox::new(
            sid.clone(),
            entry.container_id,
            entry.volume_name,
            tier_name.into(),
            tier_config.timeout_secs,
            metadata,
        );

        sandbox.transition(SandboxState::Starting)?;
        sandbox.transition(SandboxState::Running)?;

        self.sandboxes.insert(sid, sandbox.clone());

        tracing::info!(
            sandbox = %sandbox.id,
            container = %sandbox.container_id,
            volume = %sandbox.volume_name,
            tier = %tier_name,
            "sandbox claimed"
        );

        Ok(sandbox)
    }

    /// Soft release: stop + remove container, but keep volume and sandbox in DashMap.
    /// Sandbox can be resumed later.
    pub async fn release(&self, sandbox_id: &str) -> Result<(), DenError> {
        let mut entry = self
            .sandboxes
            .get_mut(sandbox_id)
            .ok_or_else(|| DenError::SandboxNotFound {
                id: sandbox_id.into(),
            })?;

        let sandbox = entry.value_mut();

        if sandbox.state == SandboxState::Running {
            sandbox.transition(SandboxState::Completed)?;
        }

        let container_id = sandbox.container_id.clone();
        drop(entry);

        // Stop and remove container, volume stays
        let _ = self.docker.stop_container(&container_id, 5).await;
        let _ = self.docker.remove_container(&container_id).await;

        tracing::info!(sandbox = %sandbox_id, "sandbox released (volume kept)");
        Ok(())
    }

    /// Resume a stopped sandbox: create new container mounting existing volume.
    /// Holds write lock across check+update to prevent race with reaper.
    pub async fn resume(&self, sandbox_id: &str) -> Result<Sandbox, DenError> {
        // Take write lock immediately to prevent reaper from destroying between check and update
        let mut entry = self
            .sandboxes
            .get_mut(sandbox_id)
            .ok_or_else(|| DenError::SandboxNotFound {
                id: sandbox_id.into(),
            })?;

        let sandbox = entry.value();
        if sandbox.state == SandboxState::Running {
            return Err(DenError::InvalidState {
                id: sandbox_id.into(),
                current: sandbox.state,
                operation: "resume (already running)".into(),
            });
        }

        let volume_name = sandbox.volume_name.clone();
        let tier_name = sandbox.tier.clone();

        let tier_config = self
            .config
            .tiers
            .get(&tier_name)
            .ok_or_else(|| DenError::TierNotFound {
                tier: tier_name.clone(),
            })?;

        // Mark as Starting before releasing lock so reaper sees non-terminal state
        let sandbox = entry.value_mut();
        sandbox.state = SandboxState::Pending;
        sandbox.transition(SandboxState::Starting)?;
        drop(entry);

        // Create + start new container (no lock held during I/O)
        let name = container_name(&tier_name);
        let container_id = self
            .docker
            .create_container(tier_config, &name, &volume_name)
            .await?;
        self.docker.start_container(&container_id).await?;

        // Re-acquire lock, finish transition
        let mut entry = self
            .sandboxes
            .get_mut(sandbox_id)
            .ok_or_else(|| DenError::SandboxNotFound {
                id: sandbox_id.into(),
            })?;

        let sandbox = entry.value_mut();
        sandbox.container_id = container_id;
        sandbox.transition(SandboxState::Running)?;

        let result = sandbox.clone();

        tracing::info!(
            sandbox = %sandbox_id,
            container = %result.container_id,
            volume = %volume_name,
            "sandbox resumed"
        );

        Ok(result)
    }

    /// Full destroy: stop container, remove container, remove volume, remove from DashMap.
    /// No recovery after this.
    pub async fn destroy(&self, sandbox_id: &str) -> Result<(), DenError> {
        let sandbox = self
            .sandboxes
            .remove(sandbox_id)
            .map(|(_, s)| s)
            .ok_or_else(|| DenError::SandboxNotFound {
                id: sandbox_id.into(),
            })?;

        let _ = self.docker.stop_container(&sandbox.container_id, 5).await;
        let _ = self.docker.remove_container(&sandbox.container_id).await;
        let _ = self.docker.remove_volume(&sandbox.volume_name).await;

        tracing::info!(
            sandbox = %sandbox_id,
            volume = %sandbox.volume_name,
            "sandbox destroyed (volume removed)"
        );
        Ok(())
    }

    /// Top up each tier's available pool back to target size.
    pub async fn replenish(&self) {
        for (tier_name, tier_config) in &self.config.tiers {
            let Some(tier_pool) = self.tiers.get(tier_name) else {
                continue;
            };

            let deficit = {
                let available = tier_pool.available.lock().await;
                tier_pool.target_size.saturating_sub(available.len())
            };

            if deficit == 0 {
                continue;
            }

            tracing::info!(tier = %tier_name, deficit, "replenishing pool");

            let mut handles = Vec::with_capacity(deficit);
            for _ in 0..deficit {
                let docker = self.docker.clone();
                let tier = tier_config.clone();
                let name = container_name(tier_name);
                let vol = volume_name();
                handles.push(tokio::spawn(async move {
                    docker.create_volume(&vol).await?;
                    let cid = docker.create_container(&tier, &name, &vol).await?;
                    Ok::<_, DenError>((cid, vol))
                }));
            }

            let mut available = tier_pool.available.lock().await;
            for handle in handles {
                match handle.await {
                    Ok(Ok((cid, vol))) => available.push(PoolEntry {
                        container_id: cid,
                        volume_name: vol,
                    }),
                    Ok(Err(e)) => {
                        tracing::warn!(tier = %tier_name, error = %e, "replenish failed");
                    }
                    Err(e) => {
                        tracing::warn!(tier = %tier_name, error = %e, "replenish spawn failed");
                    }
                }
            }
        }
    }
}

fn container_name(tier: &str) -> String {
    let short = &uuid::Uuid::new_v4().to_string()[..8];
    format!("den-{tier}-{short}")
}

fn volume_name() -> String {
    let short = &uuid::Uuid::new_v4().to_string()[..8];
    format!("den-vol-{short}")
}

fn sandbox_id(tier: &str) -> String {
    format!("{tier}-{}", uuid::Uuid::new_v4())
}

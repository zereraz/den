use std::collections::HashMap;

use serde::Deserialize;

use crate::error::DenError;

#[derive(Debug, Deserialize, Clone)]
pub struct DenConfig {
    pub server: ServerConfig,
    #[serde(default)]
    pub docker: DockerConfig,
    pub pool: PoolConfig,
    pub reaper: ReaperConfig,
    #[serde(default)]
    pub tiers: HashMap<String, TierConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub api_key: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct DockerConfig {
    pub socket: Option<String>,
    pub network: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PoolConfig {
    #[serde(default = "default_replenish_interval")]
    pub replenish_interval_secs: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ReaperConfig {
    #[serde(default = "default_reaper_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_volume_ttl")]
    pub volume_ttl_secs: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TierConfig {
    pub image: String,
    pub memory_mb: u64,
    pub cpus: f64,
    #[serde(default = "default_pids")]
    pub pids: i64,
    pub timeout_secs: u64,
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,
    #[serde(default)]
    pub readonly_rootfs: bool,
    #[serde(default)]
    pub tmpfs: Option<HashMap<String, String>>,
    /// Max concurrent exec requests per sandbox. 429 when exceeded. Default: 4.
    #[serde(default = "default_max_concurrent_execs")]
    pub max_concurrent_execs: usize,
}

fn default_host() -> String {
    "0.0.0.0".into()
}
fn default_port() -> u16 {
    8080
}
fn default_replenish_interval() -> u64 {
    5
}
fn default_reaper_interval() -> u64 {
    10
}
fn default_idle_timeout() -> u64 {
    300
}
fn default_volume_ttl() -> u64 {
    86400 // 24 hours
}
fn default_pids() -> i64 {
    256
}
fn default_pool_size() -> usize {
    3
}
fn default_max_concurrent_execs() -> usize {
    4
}

pub fn load(path: &str) -> Result<DenConfig, DenError> {
    let content =
        std::fs::read_to_string(path).map_err(|e| DenError::Config(format!("{path}: {e}")))?;
    let config: DenConfig =
        toml::from_str(&content).map_err(|e| DenError::Config(format!("{path}: {e}")))?;
    validate(&config)?;
    Ok(config)
}

fn validate(config: &DenConfig) -> Result<(), DenError> {
    if config.tiers.is_empty() {
        return Err(DenError::Config("at least one tier required".into()));
    }
    for (name, tier) in &config.tiers {
        if tier.memory_mb == 0 {
            return Err(DenError::Config(format!("tier {name}: memory_mb must be > 0")));
        }
        if tier.cpus <= 0.0 {
            return Err(DenError::Config(format!("tier {name}: cpus must be > 0")));
        }
        if tier.image.is_empty() {
            return Err(DenError::Config(format!("tier {name}: image required")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_config() {
        let toml = r#"
[server]
host = "127.0.0.1"
port = 9090

[pool]
replenish_interval_secs = 3

[reaper]
interval_secs = 5
idle_timeout_secs = 60

[tiers.test]
image = "ubuntu:24.04"
memory_mb = 256
cpus = 0.5
timeout_secs = 300
pool_size = 2
"#;
        let config: DenConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.server.port, 9090);
        assert_eq!(config.tiers.len(), 1);
        assert_eq!(config.tiers["test"].memory_mb, 256);
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn reject_zero_memory() {
        let toml = r#"
[server]
[pool]
replenish_interval_secs = 5
[reaper]
interval_secs = 10
idle_timeout_secs = 300
[tiers.bad]
image = "ubuntu:24.04"
memory_mb = 0
cpus = 1.0
timeout_secs = 60
"#;
        let config: DenConfig = toml::from_str(toml).unwrap();
        assert!(validate(&config).is_err());
    }

    #[test]
    fn reject_no_tiers() {
        let toml = r#"
[server]
[pool]
replenish_interval_secs = 5
[reaper]
interval_secs = 10
idle_timeout_secs = 300
"#;
        let config: DenConfig = toml::from_str(toml).unwrap();
        assert!(validate(&config).is_err());
    }
}

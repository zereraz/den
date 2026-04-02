use std::collections::HashMap;
use std::io::{Cursor, Read as _};
use std::path::Path;
use std::pin::Pin;

use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    CreateContainerOptions, RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};
use bollard::models::VolumeCreateOptions;
use bollard::Docker;
use bytes::Bytes;
use futures_util::StreamExt;
use tokio::io::AsyncWrite;

use crate::config::TierConfig;
use crate::error::DenError;

pub struct DockerManager {
    client: Docker,
    network: Option<String>,
}

#[derive(Debug)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i64,
}

pub struct ExecStream {
    pub exec_id: String,
    pub output: Pin<Box<dyn futures_util::Stream<Item = Result<LogOutput, bollard::errors::Error>> + Send>>,
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
}

impl DockerManager {
    pub fn new(socket: Option<&str>, network: Option<String>) -> Result<Self, DenError> {
        let client = match socket {
            Some(path) => Docker::connect_with_socket(path, 120, bollard::API_DEFAULT_VERSION)?,
            None => Docker::connect_with_socket_defaults()?,
        };
        Ok(Self { client, network })
    }

    pub fn client(&self) -> &Docker {
        &self.client
    }

    pub async fn create_volume(&self, name: &str) -> Result<(), DenError> {
        let mut labels: HashMap<String, String> = HashMap::new();
        labels.insert("den.managed".into(), "true".into());

        let options = VolumeCreateOptions {
            name: Some(name.into()),
            labels: Some(labels),
            ..Default::default()
        };
        self.client.create_volume(options).await?;
        tracing::info!(volume = %name, "volume created");
        Ok(())
    }

    pub async fn remove_volume(&self, name: &str) -> Result<(), DenError> {
        self.client
            .remove_volume(
                name,
                None::<bollard::query_parameters::RemoveVolumeOptions>,
            )
            .await?;
        tracing::info!(volume = %name, "volume removed");
        Ok(())
    }

    pub async fn create_container(
        &self,
        tier: &TierConfig,
        name: &str,
        volume_name: &str,
        extra_binds: &[String],
    ) -> Result<String, DenError> {
        let mut binds = vec![format!("{volume_name}:/home/sandbox")];
        binds.extend(extra_binds.iter().cloned());

        let host_config = HostConfig {
            memory: Some((tier.memory_mb * 1024 * 1024) as i64),
            nano_cpus: Some((tier.cpus * 1e9) as i64),
            pids_limit: Some(tier.pids),
            cap_drop: Some(vec!["ALL".into()]),
            cap_add: Some(vec!["NET_BIND_SERVICE".into()]),
            security_opt: Some(vec!["no-new-privileges:true".into()]),
            readonly_rootfs: Some(tier.readonly_rootfs),
            tmpfs: tier.tmpfs.clone(),
            network_mode: self.network.clone(),
            binds: Some(binds),
            ..Default::default()
        };

        let mut labels = HashMap::new();
        labels.insert("den.managed".into(), "true".into());

        let body = ContainerCreateBody {
            image: Some(tier.image.clone()),
            host_config: Some(host_config),
            labels: Some(labels),
            ..Default::default()
        };

        let options = CreateContainerOptions {
            name: Some(name.into()),
            ..Default::default()
        };

        let response = self.client.create_container(Some(options), body).await?;
        tracing::info!(container = %response.id, name, volume = %volume_name, "container created");
        Ok(response.id)
    }

    pub async fn start_container(&self, id: &str) -> Result<(), DenError> {
        self.client
            .start_container(id, None::<StartContainerOptions>)
            .await?;
        tracing::info!(container = %id, "container started");
        Ok(())
    }

    pub async fn stop_container(&self, id: &str, timeout_secs: i32) -> Result<(), DenError> {
        let options = StopContainerOptions {
            t: Some(timeout_secs),
            ..Default::default()
        };
        self.client.stop_container(id, Some(options)).await?;
        tracing::info!(container = %id, "container stopped");
        Ok(())
    }

    pub async fn remove_container(&self, id: &str) -> Result<(), DenError> {
        let options = RemoveContainerOptions {
            force: true,
            v: false, // we manage named volumes ourselves
            ..Default::default()
        };
        self.client.remove_container(id, Some(options)).await?;
        tracing::info!(container = %id, "container removed");
        Ok(())
    }

    /// Run a command and collect all output. Used by REST exec endpoint.
    pub async fn exec_collect(
        &self,
        container_id: &str,
        cmd: Vec<String>,
        workdir: Option<String>,
        env: Option<Vec<String>>,
    ) -> Result<ExecResult, DenError> {
        let exec = self
            .client
            .create_exec(
                container_id,
                CreateExecOptions {
                    cmd: Some(cmd),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    working_dir: workdir,
                    env,
                    ..Default::default()
                },
            )
            .await?;

        let start = self
            .client
            .start_exec(&exec.id, None::<bollard::exec::StartExecOptions>)
            .await?;

        let mut stdout = String::new();
        let mut stderr = String::new();

        if let StartExecResults::Attached { mut output, .. } = start {
            while let Some(Ok(msg)) = output.next().await {
                match msg {
                    LogOutput::StdOut { message } => {
                        stdout.push_str(&String::from_utf8_lossy(&message));
                    }
                    LogOutput::StdErr { message } => {
                        stderr.push_str(&String::from_utf8_lossy(&message));
                    }
                    _ => {}
                }
            }
        }

        let inspect = self.client.inspect_exec(&exec.id).await?;
        let exit_code = inspect.exit_code.unwrap_or(-1);

        Ok(ExecResult {
            stdout,
            stderr,
            exit_code,
        })
    }

    /// Start a command with bidirectional streaming. Used by WebSocket exec endpoint.
    pub async fn exec_stream(
        &self,
        container_id: &str,
        cmd: Vec<String>,
        workdir: Option<String>,
        env: Option<Vec<String>>,
    ) -> Result<ExecStream, DenError> {
        let exec = self
            .client
            .create_exec(
                container_id,
                CreateExecOptions {
                    cmd: Some(cmd),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    attach_stdin: Some(true),
                    working_dir: workdir,
                    env,
                    ..Default::default()
                },
            )
            .await?;

        let exec_id = exec.id.clone();
        let start = self
            .client
            .start_exec(&exec.id, None::<bollard::exec::StartExecOptions>)
            .await?;

        match start {
            StartExecResults::Attached { output, input } => Ok(ExecStream {
                exec_id,
                output,
                input,
            }),
            StartExecResults::Detached => Err(anyhow::anyhow!("expected attached exec").into()),
        }
    }

    /// Get exit code for a completed exec.
    pub async fn inspect_exec(&self, exec_id: &str) -> Result<i64, DenError> {
        let inspect = self.client.inspect_exec(exec_id).await?;
        Ok(inspect.exit_code.unwrap_or(-1))
    }

    /// Get container IP address from Docker network settings.
    pub async fn get_container_ip(&self, id: &str) -> Result<String, DenError> {
        let inspect = self
            .client
            .inspect_container(
                id,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await?;

        let networks = inspect
            .network_settings
            .and_then(|ns| ns.networks)
            .ok_or_else(|| DenError::Proxy(format!("no network settings for container {id}")))?;

        let ip = if let Some(ref net_name) = self.network {
            networks.get(net_name).and_then(|n| n.ip_address.clone())
        } else {
            networks.values().next().and_then(|n| n.ip_address.clone())
        };

        ip.filter(|s| !s.is_empty())
            .ok_or_else(|| DenError::Proxy(format!("no IP for container {id}")))
    }

    /// Download a single file from a container. Returns raw bytes.
    pub async fn download_file(
        &self,
        container_id: &str,
        file_path: &str,
    ) -> Result<Vec<u8>, DenError> {
        let options = bollard::query_parameters::DownloadFromContainerOptionsBuilder::default()
            .path(file_path)
            .build();

        let mut stream = self.client.download_from_container(container_id, Some(options));
        let mut tar_bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| match &e {
                bollard::errors::Error::DockerResponseServerError { status_code: 404, .. } => {
                    DenError::FileNotFound { path: file_path.to_string() }
                }
                _ => DenError::Docker(e),
            })?;
            tar_bytes.extend_from_slice(&chunk);
        }

        // Extract the single file from the tar archive
        let cursor = Cursor::new(tar_bytes);
        let mut archive = tar::Archive::new(cursor);
        let entries = archive.entries().map_err(|e| anyhow::anyhow!("tar read: {e}"))?;
        for entry in entries {
            let mut entry = entry.map_err(|e| anyhow::anyhow!("tar entry: {e}"))?;
            let mut content = Vec::new();
            entry.read_to_end(&mut content).map_err(|e| anyhow::anyhow!("tar read: {e}"))?;
            return Ok(content);
        }

        Err(anyhow::anyhow!("file not found in archive: {file_path}").into())
    }

    /// Upload a single file to a container. Takes raw bytes.
    pub async fn upload_file(
        &self,
        container_id: &str,
        file_path: &str,
        content: &[u8],
    ) -> Result<(), DenError> {
        let path = Path::new(file_path);
        let parent = path.parent().unwrap_or(Path::new("/"));
        let file_name = path.file_name()
            .ok_or_else(|| anyhow::anyhow!("invalid file path: {file_path}"))?;

        // Build a tar archive containing the single file
        let mut tar_builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar_builder.append_data(
            &mut header,
            file_name,
            content,
        ).map_err(|e| anyhow::anyhow!("tar append: {e}"))?;
        let tar_bytes = tar_builder.into_inner().map_err(|e| anyhow::anyhow!("tar finalize: {e}"))?;

        let options = bollard::query_parameters::UploadToContainerOptionsBuilder::default()
            .path(&parent.to_string_lossy())
            .build();

        self.client
            .upload_to_container(
                container_id,
                Some(options),
                bollard::body_full(Bytes::from(tar_bytes)),
            )
            .await?;

        Ok(())
    }

    /// Check if a container is still running in Docker.
    pub async fn container_is_running(&self, id: &str) -> Result<bool, DenError> {
        let inspect = self
            .client
            .inspect_container(
                id,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await?;
        Ok(inspect.state.and_then(|s| s.running).unwrap_or(false))
    }

    /// Check if container was OOM-killed by Docker. Used after exit_code 137.
    pub async fn container_oom_killed(&self, container_id: &str) -> Result<bool, DenError> {
        let inspect = self
            .client
            .inspect_container(
                container_id,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await?;
        Ok(inspect
            .state
            .and_then(|s| s.oom_killed)
            .unwrap_or(false))
    }

    /// Get single-shot container resource stats for diagnostics.
    pub async fn container_stats(&self, container_id: &str) -> Result<ContainerDiagnostics, DenError> {
        use bollard::query_parameters::StatsOptions;
        use futures_util::TryStreamExt;

        let options = StatsOptions {
            stream: false,
            one_shot: true,
            ..Default::default()
        };

        let stats: Vec<_> = self
            .client
            .stats(container_id, Some(options))
            .try_collect()
            .await?;

        let stat = stats
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no stats returned for container {container_id}"))?;

        let memory_usage = stat.memory_stats.as_ref().and_then(|m| m.usage).unwrap_or(0);
        let memory_limit = stat.memory_stats.as_ref().and_then(|m| m.limit).unwrap_or(0);
        let memory_percent = if memory_limit > 0 {
            (memory_usage as f64 / memory_limit as f64) * 100.0
        } else {
            0.0
        };

        let pids_current = stat.pids_stats.as_ref().and_then(|p| p.current).unwrap_or(0);
        let pids_limit = stat.pids_stats.as_ref().and_then(|p| p.limit).unwrap_or(0);

        // CPU percent: delta usage / delta system * num_cpus * 100
        let cpu_percent = stat.cpu_stats.as_ref().map(|cpu| {
            let cpu_usage = cpu.cpu_usage.as_ref()
                .and_then(|u| u.total_usage)
                .unwrap_or(0) as f64;
            let precpu_usage = stat.precpu_stats.as_ref()
                .and_then(|p| p.cpu_usage.as_ref())
                .and_then(|u| u.total_usage)
                .unwrap_or(0) as f64;
            let cpu_delta = cpu_usage - precpu_usage;
            let system_delta = cpu.system_cpu_usage.unwrap_or(0) as f64
                - stat.precpu_stats.as_ref()
                    .and_then(|p| p.system_cpu_usage)
                    .unwrap_or(0) as f64;
            let num_cpus = cpu.online_cpus.unwrap_or(1) as f64;
            if system_delta > 0.0 {
                (cpu_delta / system_delta) * num_cpus * 100.0
            } else {
                0.0
            }
        }).unwrap_or(0.0);

        Ok(ContainerDiagnostics {
            memory_usage_bytes: memory_usage,
            memory_limit_bytes: memory_limit,
            memory_percent: (memory_percent * 10.0).round() / 10.0, // 1 decimal
            pids_current,
            pids_limit,
            cpu_percent: (cpu_percent * 10.0).round() / 10.0,
        })
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ContainerDiagnostics {
    pub memory_usage_bytes: u64,
    pub memory_limit_bytes: u64,
    pub memory_percent: f64,
    pub pids_current: u64,
    pub pids_limit: u64,
    pub cpu_percent: f64,
}

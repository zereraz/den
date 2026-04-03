mod api;
mod config;
mod docker;
mod error;
mod pool;
mod proxy;
mod reaper;
mod sandbox;
mod scheduler;

use std::sync::Arc;

use clap::Parser;
use dashmap::DashMap;
use tokio::sync::watch;

#[derive(Parser)]
#[command(name = "den", about = "Lightweight container runtime for AI agent sandboxes")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "den.toml")]
    config: String,

    /// Override server port
    #[arg(short, long)]
    port: Option<u16>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Init tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "den=info".into()),
        )
        .init();

    let cli = Cli::parse();

    // Load config
    let mut config = config::load(&cli.config)?;
    if let Some(port) = cli.port {
        config.server.port = port;
    }
    let config = Arc::new(config);

    tracing::info!("den v{}", env!("CARGO_PKG_VERSION"));

    // Docker
    let docker = Arc::new(docker::DockerManager::new(
        config.docker.socket.as_deref(),
        config.docker.network.clone(),
    )?);

    // Verify Docker is reachable
    docker.client().ping().await.map_err(|e| {
        anyhow::anyhow!("cannot reach Docker daemon: {e}")
    })?;
    tracing::info!("docker daemon connected");

    // Shared sandbox state
    let sandboxes = Arc::new(DashMap::new());

    // Scheduler (created before pool so pool can use it)
    let scheduler = Arc::new(scheduler::Scheduler::new(docker.clone(), config.clone()));

    // Pool
    let pool = Arc::new(pool::Pool::new(
        docker.clone(),
        config.clone(),
        sandboxes.clone(),
        scheduler.clone(),
    ));

    // Clean up orphans from previous runs
    cleanup_orphans(&docker, &sandboxes).await;

    // Warm up pool
    tracing::info!("warming up container pool...");
    pool.warm_up().await?;

    // Shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Spawn reaper
    let reaper = reaper::Reaper::new(sandboxes.clone(), docker.clone(), config.clone());
    let reaper_handle = tokio::spawn(async move {
        reaper.run(shutdown_rx).await;
    });

    // Spawn pool replenisher
    let replenish_pool = pool.clone();
    let replenish_config = config.clone();
    let mut replenish_shutdown = shutdown_tx.subscribe();
    let replenish_handle = tokio::spawn(async move {
        let interval =
            std::time::Duration::from_secs(replenish_config.pool.replenish_interval_secs);
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    replenish_pool.replenish().await;
                }
                _ = replenish_shutdown.changed() => {
                    tracing::info!("replenisher shutting down");
                    return;
                }
            }
        }
    });

    // Build app state and router
    let app_state = Arc::new(api::AppState {
        pool: pool.clone(),
        docker: docker.clone(),
        scheduler,
        exec_semaphores: dashmap::DashMap::new(),
    });

    let app = api::router(app_state)
        .layer(tower_http::cors::CorsLayer::permissive())
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on {addr}");

    // Serve with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to listen for ctrl+c");
            tracing::info!("shutdown signal received");
            let _ = shutdown_tx.send(true);
        })
        .await?;

    // Wait for background tasks
    let _ = tokio::join!(reaper_handle, replenish_handle);

    tracing::info!("den stopped");
    Ok(())
}

/// Remove orphaned containers and volumes from previous den runs.
/// Finds anything with label `den.managed=true` that isn't tracked in state.
async fn cleanup_orphans(
    docker: &docker::DockerManager,
    _sandboxes: &dashmap::DashMap<String, sandbox::Sandbox>,
) {
    use std::collections::HashMap as StdMap;

    // Orphan containers
    let mut c_filters: StdMap<String, Vec<String>> = StdMap::new();
    c_filters.insert("label".into(), vec!["den.managed=true".into()]);
    let opts = bollard::query_parameters::ListContainersOptions {
        all: true,
        filters: Some(c_filters),
        ..Default::default()
    };
    match docker.client().list_containers(Some(opts)).await {
        Ok(containers) => {
            for c in containers {
                let id = c.id.unwrap_or_default();
                if id.is_empty() { continue; }
                tracing::warn!(container = %id, "removing orphan container");
                let _ = docker.stop_container(&id, 2).await;
                let _ = docker.remove_container(&id).await;
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to list orphan containers"),
    }

    // Orphan volumes
    let mut v_filters: StdMap<String, Vec<String>> = StdMap::new();
    v_filters.insert("label".into(), vec!["den.managed=true".into()]);
    let opts = bollard::query_parameters::ListVolumesOptions {
        filters: Some(v_filters),
        ..Default::default()
    };
    match docker.client().list_volumes(Some(opts)).await {
        Ok(resp) => {
            if let Some(volumes) = resp.volumes {
                for v in volumes {
                    let name = v.name;
                    tracing::warn!(volume = %name, "removing orphan volume");
                    let _ = docker.remove_volume(&name).await;
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to list orphan volumes"),
    }
}

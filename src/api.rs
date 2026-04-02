use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::docker::DockerManager;
use crate::error::DenError;
use crate::pool::Pool;
use crate::sandbox::Sandbox;
use crate::scheduler::Scheduler;

pub struct AppState {
    pub pool: Arc<Pool>,
    pub docker: Arc<DockerManager>,
    pub scheduler: Arc<Scheduler>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/sandboxes", post(create_sandbox))
        .route("/api/v1/sandboxes", get(list_sandboxes))
        .route("/api/v1/sandboxes/{id}", get(get_sandbox))
        .route("/api/v1/sandboxes/{id}", delete(delete_sandbox))
        .route("/api/v1/sandboxes/{id}/resume", post(resume_sandbox))
        .route("/api/v1/sandboxes/{id}/volume", delete(destroy_sandbox))
        .route("/api/v1/sandboxes/{id}/exec", post(exec_rest))
        .route("/api/v1/sandboxes/{id}/exec/ws", get(exec_ws))
        .route("/api/v1/sandboxes/{id}/diagnostics", get(diagnostics))
        .route("/api/v1/sandboxes/{id}/files/{*path}", get(download_file))
        .route("/api/v1/sandboxes/{id}/files/{*path}", put(upload_file))
        .route(
            "/proxy/{id}/{port}/{*rest}",
            axum::routing::any(crate::proxy::proxy_handler),
        )
        .route("/api/v1/resources", get(get_resources))
        .route("/api/v1/health", get(health))
        .layer(axum::extract::DefaultBodyLimit::max(100 * 1024 * 1024)) // 100 MB
        .with_state(state)
}

// --- Request / Response types ---

#[derive(Deserialize)]
struct CreateRequest {
    tier: String,
    #[serde(default)]
    metadata: HashMap<String, String>,
    /// Host bind mounts (e.g. ["/host/path:/container/path:rw"]).
    /// When present, the sandbox is created on-demand (dedicated mode) instead
    /// of being claimed from the pre-warmed pool.
    #[serde(default)]
    bind_mounts: Vec<String>,
}

#[derive(Serialize)]
struct SandboxResponse {
    id: String,
    container_id: String,
    tier: String,
    state: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    bind_mounts: Vec<String>,
}

impl From<Sandbox> for SandboxResponse {
    fn from(s: Sandbox) -> Self {
        Self {
            id: s.id,
            container_id: s.container_id,
            tier: s.tier,
            state: format!("{:?}", s.state).to_lowercase(),
            bind_mounts: s.bind_mounts,
        }
    }
}

#[derive(Deserialize)]
struct ExecRequest {
    cmd: Vec<String>,
    workdir: Option<String>,
    env: Option<Vec<String>>,
    timeout_secs: Option<u64>,
}

#[derive(Serialize)]
struct ExecResponse {
    stdout: String,
    stderr: String,
    exit_code: i64,
    oom_killed: bool,
}

// --- WS message types ---

#[derive(Deserialize)]
struct WsExecStart {
    cmd: Vec<String>,
    workdir: Option<String>,
    env: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum WsClientMsg {
    #[serde(rename = "stdin")]
    Stdin { data: String },
}

#[derive(Serialize)]
struct WsServerMsg {
    #[serde(rename = "type")]
    msg_type: String,
    data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    oom_killed: Option<bool>,
}

// --- Handlers ---

/// Blocked host paths for bind mounts.
const BLOCKED_BIND_PREFIXES: &[&str] = &[
    "/var/run/docker",
    "/proc",
    "/sys",
    "/dev",
    "/etc",
    "/root",
];

/// Validate a bind mount string. Must be "host:container" or "host:container:mode".
/// Host and container paths must be absolute. Host path must not target blocked prefixes.
fn validate_bind_mount(mount: &str) -> Result<(), DenError> {
    let parts: Vec<&str> = mount.splitn(3, ':').collect();
    if parts.len() < 2 {
        return Err(DenError::Other(anyhow::anyhow!(
            "invalid bind mount format (expected host:container[:mode]): {mount}"
        )));
    }
    let host_path = parts[0];
    let container_path = parts[1];

    if !host_path.starts_with('/') || !container_path.starts_with('/') {
        return Err(DenError::Other(anyhow::anyhow!(
            "bind mount paths must be absolute: {mount}"
        )));
    }
    if container_path == "/home/sandbox" {
        return Err(DenError::Other(anyhow::anyhow!(
            "bind mount cannot target /home/sandbox (reserved for volume): {mount}"
        )));
    }
    for prefix in BLOCKED_BIND_PREFIXES {
        if host_path.starts_with(prefix) {
            return Err(DenError::Other(anyhow::anyhow!(
                "bind mount host path blocked ({prefix}): {mount}"
            )));
        }
    }
    if parts.len() == 3 && !matches!(parts[2], "ro" | "rw") {
        return Err(DenError::Other(anyhow::anyhow!(
            "bind mount mode must be 'ro' or 'rw': {mount}"
        )));
    }
    Ok(())
}

async fn create_sandbox(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateRequest>,
) -> Result<Json<SandboxResponse>, DenError> {
    for mount in &req.bind_mounts {
        validate_bind_mount(mount)?;
    }
    let sandbox = state.pool.claim(&req.tier, req.metadata, req.bind_mounts).await?;
    Ok(Json(sandbox.into()))
}

async fn list_sandboxes(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<SandboxResponse>> {
    let sandboxes: Vec<SandboxResponse> = state
        .pool
        .sandboxes
        .iter()
        .map(|entry| entry.value().clone().into())
        .collect();
    Json(sandboxes)
}

async fn get_sandbox(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SandboxResponse>, DenError> {
    let sandbox = state
        .pool
        .sandboxes
        .get(&id)
        .map(|entry| entry.value().clone())
        .ok_or_else(|| DenError::SandboxNotFound { id: id.clone() })?;
    Ok(Json(sandbox.into()))
}

async fn delete_sandbox(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, DenError> {
    state.pool.release(&id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn resume_sandbox(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SandboxResponse>, DenError> {
    let sandbox = state.pool.resume(&id).await?;
    Ok(Json(sandbox.into()))
}

async fn destroy_sandbox(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, DenError> {
    state.pool.destroy(&id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn exec_rest(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ExecRequest>,
) -> Result<Json<ExecResponse>, DenError> {
    let container_id = {
        let entry = state
            .pool
            .sandboxes
            .get(&id)
            .ok_or_else(|| DenError::SandboxNotFound { id: id.clone() })?;
        // Touch to update last_activity
        drop(entry);
        let mut entry = state
            .pool
            .sandboxes
            .get_mut(&id)
            .ok_or_else(|| DenError::SandboxNotFound { id: id.clone() })?;
        entry.value_mut().touch();
        entry.value().container_id.clone()
    };

    const MAX_EXEC_TIMEOUT: u64 = 300;
    let timeout = req.timeout_secs.unwrap_or(30).min(MAX_EXEC_TIMEOUT);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout),
        state
            .docker
            .exec_collect(&container_id, req.cmd, req.workdir, req.env),
    )
    .await
    .map_err(|_| DenError::Timeout { seconds: timeout })?
    .map_err(DenError::from)?;

    // Check OOM on signal-killed processes (137 = SIGKILL, typical OOM)
    let oom_killed = if result.exit_code == 137 {
        state.docker.container_oom_killed(&container_id).await.unwrap_or(false)
    } else {
        false
    };

    Ok(Json(ExecResponse {
        stdout: result.stdout,
        stderr: result.stderr,
        exit_code: result.exit_code,
        oom_killed,
    }))
}

async fn exec_ws(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, DenError> {
    let container_id = {
        let entry = state
            .pool
            .sandboxes
            .get(&id)
            .ok_or_else(|| DenError::SandboxNotFound { id: id.clone() })?;
        entry.value().container_id.clone()
    };

    Ok(ws
        .max_message_size(64 * 1024) // 64 KB max per WS message
        .on_upgrade(move |socket| handle_ws(socket, state, id, container_id)))
}

/// Max WS exec session duration (1 hour).
const WS_EXEC_TIMEOUT_SECS: u64 = 3600;

async fn handle_ws(
    mut socket: WebSocket,
    state: Arc<AppState>,
    sandbox_id: String,
    container_id: String,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::AsyncWriteExt;

    // First message from client should be the exec start command
    let start_msg = match socket.recv().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<WsExecStart>(&text) {
            Ok(msg) => msg,
            Err(e) => {
                let _ = socket
                    .send(Message::Text(
                        serde_json::to_string(&WsServerMsg {
                            msg_type: "error".into(),
                            data: format!("invalid start message: {e}"),
                            oom_killed: None,
                        })
                        .unwrap()
                        .into(),
                    ))
                    .await;
                return;
            }
        },
        _ => return,
    };

    let exec_stream = match state
        .docker
        .exec_stream(&container_id, start_msg.cmd, start_msg.workdir, start_msg.env)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            let _ = socket
                .send(Message::Text(
                    serde_json::to_string(&WsServerMsg {
                        msg_type: "error".into(),
                        data: e.to_string(),
                        oom_killed: None,
                    })
                    .unwrap()
                    .into(),
                ))
                .await;
            return;
        }
    };

    let exec_id = exec_stream.exec_id.clone();
    let mut output = exec_stream.output;
    let mut input = exec_stream.input;

    let (ws_tx, ws_rx) = socket.split();

    // Forward container output → WebSocket
    let docker_clone = state.docker.clone();
    let cid = container_id.clone();
    let output_task = tokio::spawn(async move {
        use bollard::container::LogOutput;

        let mut ws_tx = ws_tx;

        while let Some(Ok(msg)) = output.next().await {
            let (msg_type, data) = match msg {
                LogOutput::StdOut { message } => {
                    ("stdout", String::from_utf8_lossy(&message).into_owned())
                }
                LogOutput::StdErr { message } => {
                    ("stderr", String::from_utf8_lossy(&message).into_owned())
                }
                _ => continue,
            };

            let json = serde_json::to_string(&WsServerMsg {
                msg_type: msg_type.into(),
                data,
                oom_killed: None,
            })
            .unwrap();

            if ws_tx.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }

        // Send exit code with OOM detection
        let exit_code = docker_clone
            .inspect_exec(&exec_id)
            .await
            .unwrap_or(-1);

        let oom = if exit_code == 137 {
            docker_clone.container_oom_killed(&cid).await.unwrap_or(false)
        } else {
            false
        };

        let _ = ws_tx
            .send(Message::Text(
                serde_json::to_string(&WsServerMsg {
                    msg_type: "exit".into(),
                    data: exit_code.to_string(),
                    oom_killed: if oom { Some(true) } else { None },
                })
                .unwrap()
                .into(),
            ))
            .await;
    });

    // Forward WebSocket stdin → container input
    let sandbox_ref = state.pool.sandboxes.clone();
    let sid = sandbox_id.clone();
    let input_task = tokio::spawn(async move {
        let mut ws_rx = ws_rx;

        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Text(text) => {
                    if let Ok(WsClientMsg::Stdin { data }) =
                        serde_json::from_str::<WsClientMsg>(&text)
                    {
                        if input.write_all(data.as_bytes()).await.is_err() {
                            break;
                        }
                        // Touch sandbox on activity
                        if let Some(mut entry) = sandbox_ref.get_mut(&sid) {
                            entry.value_mut().touch();
                        }
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // Enforce max session timeout
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(WS_EXEC_TIMEOUT_SECS),
        async { tokio::join!(output_task, input_task) },
    )
    .await;
}

async fn diagnostics(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<crate::docker::ContainerDiagnostics>, DenError> {
    let container_id = {
        let entry = state
            .pool
            .sandboxes
            .get(&id)
            .ok_or_else(|| DenError::SandboxNotFound { id: id.clone() })?;
        entry.value().container_id.clone()
    };

    let diag = state.docker.container_stats(&container_id).await?;
    Ok(Json(diag))
}

/// Validate file path stays within /home/sandbox. Returns the sanitized absolute path.
fn sanitize_file_path(raw: &str) -> Result<String, DenError> {
    use std::path::{Component, PathBuf};

    let raw = raw.trim_start_matches('/');
    if raw.is_empty() {
        return Err(DenError::Other(anyhow::anyhow!("empty file path")));
    }

    let base = PathBuf::from("/home/sandbox");
    let mut resolved = base.clone();

    for component in std::path::Path::new(raw).components() {
        match component {
            Component::Normal(c) => resolved.push(c),
            Component::CurDir => {} // skip "."
            Component::ParentDir => {
                resolved.pop();
                if !resolved.starts_with(&base) {
                    return Err(DenError::Other(anyhow::anyhow!(
                        "path traversal blocked"
                    )));
                }
            }
            _ => {
                return Err(DenError::Other(anyhow::anyhow!(
                    "invalid path component"
                )));
            }
        }
    }

    if !resolved.starts_with(&base) {
        return Err(DenError::Other(anyhow::anyhow!("path traversal blocked")));
    }

    Ok(resolved.to_string_lossy().into_owned())
}

async fn download_file(
    State(state): State<Arc<AppState>>,
    Path((id, file_path)): Path<(String, String)>,
) -> Result<impl IntoResponse, DenError> {
    let safe_path = sanitize_file_path(&file_path)?;

    let container_id = {
        let entry = state
            .pool
            .sandboxes
            .get(&id)
            .ok_or_else(|| DenError::SandboxNotFound { id: id.clone() })?;
        entry.value().container_id.clone()
    };

    let content = state.docker.download_file(&container_id, &safe_path).await?;

    let mime = guess_mime(&file_path);

    Ok((
        [(header::CONTENT_TYPE, mime)],
        Body::from(content),
    ))
}

async fn upload_file(
    State(state): State<Arc<AppState>>,
    Path((id, file_path)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, DenError> {
    let safe_path = sanitize_file_path(&file_path)?;

    let container_id = {
        let entry = state
            .pool
            .sandboxes
            .get(&id)
            .ok_or_else(|| DenError::SandboxNotFound { id: id.clone() })?;
        entry.value().container_id.clone()
    };

    state
        .docker
        .upload_file(&container_id, &safe_path, &body)
        .await?;

    Ok(axum::http::StatusCode::NO_CONTENT)
}

fn guess_mime(path: &str) -> String {
    match path.rsplit('.').next() {
        Some("txt" | "log" | "md" | "csv") => "text/plain; charset=utf-8",
        Some("json") => "application/json",
        Some("html" | "htm") => "text/html",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("xml") => "application/xml",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("py" | "rs" | "ts" | "tsx" | "jsx" | "sh" | "yaml" | "yml" | "toml") => {
            "text/plain; charset=utf-8"
        }
        _ => "application/octet-stream",
    }
    .to_string()
}

async fn get_resources(
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::scheduler::ResourceSnapshot>, DenError> {
    let snapshot = state.scheduler.resource_snapshot().await?;
    Ok(Json(snapshot))
}

async fn health(State(state): State<Arc<AppState>>) -> Result<&'static str, DenError> {
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.docker.client().ping(),
    )
    .await
    .map_err(|_| DenError::Other(anyhow::anyhow!("docker ping timeout")))?
    .map_err(DenError::from)?;
    Ok("ok")
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- sanitize_file_path ---

    #[test]
    fn sanitize_simple_path() {
        assert_eq!(
            sanitize_file_path("main.py").unwrap(),
            "/home/sandbox/main.py"
        );
    }

    #[test]
    fn sanitize_nested_path() {
        assert_eq!(
            sanitize_file_path("src/lib/utils.ts").unwrap(),
            "/home/sandbox/src/lib/utils.ts"
        );
    }

    #[test]
    fn sanitize_strips_leading_slash() {
        assert_eq!(
            sanitize_file_path("/readme.md").unwrap(),
            "/home/sandbox/readme.md"
        );
    }

    #[test]
    fn sanitize_blocks_traversal_to_root() {
        assert!(sanitize_file_path("../../etc/passwd").is_err());
    }

    #[test]
    fn sanitize_blocks_traversal_above_sandbox() {
        assert!(sanitize_file_path("../../../etc/shadow").is_err());
    }

    #[test]
    fn sanitize_allows_dot_dot_within_sandbox() {
        // Going up then back down within /home/sandbox is fine
        assert_eq!(
            sanitize_file_path("a/b/../c.txt").unwrap(),
            "/home/sandbox/a/c.txt"
        );
    }

    #[test]
    fn sanitize_blocks_dot_dot_escaping_sandbox() {
        // a/../../ would go to /home which is above /home/sandbox
        assert!(sanitize_file_path("a/../../secret").is_err());
    }

    #[test]
    fn sanitize_rejects_empty_path() {
        assert!(sanitize_file_path("").is_err());
        assert!(sanitize_file_path("/").is_err());
    }

    #[test]
    fn sanitize_ignores_current_dir() {
        assert_eq!(
            sanitize_file_path("./file.txt").unwrap(),
            "/home/sandbox/file.txt"
        );
    }

    #[test]
    fn sanitize_deeply_nested() {
        assert_eq!(
            sanitize_file_path("a/b/c/d/e/f.txt").unwrap(),
            "/home/sandbox/a/b/c/d/e/f.txt"
        );
    }

    // --- validate_bind_mount ---

    #[test]
    fn bind_valid_rw() {
        assert!(validate_bind_mount("/host/project:/workspace:rw").is_ok());
    }

    #[test]
    fn bind_valid_no_mode() {
        assert!(validate_bind_mount("/host/project:/workspace").is_ok());
    }

    #[test]
    fn bind_valid_ro() {
        assert!(validate_bind_mount("/data/models:/models:ro").is_ok());
    }

    #[test]
    fn bind_rejects_relative_host() {
        assert!(validate_bind_mount("relative/path:/workspace").is_err());
    }

    #[test]
    fn bind_rejects_relative_container() {
        assert!(validate_bind_mount("/host:relative").is_err());
    }

    #[test]
    fn bind_rejects_docker_socket() {
        assert!(validate_bind_mount("/var/run/docker.sock:/docker.sock").is_err());
    }

    #[test]
    fn bind_rejects_proc() {
        assert!(validate_bind_mount("/proc:/proc:ro").is_err());
    }

    #[test]
    fn bind_rejects_etc() {
        assert!(validate_bind_mount("/etc/passwd:/etc/passwd:ro").is_err());
    }

    #[test]
    fn bind_rejects_home_sandbox_target() {
        assert!(validate_bind_mount("/host:/home/sandbox").is_err());
    }

    #[test]
    fn bind_rejects_bad_mode() {
        assert!(validate_bind_mount("/host:/container:exec").is_err());
    }

    #[test]
    fn bind_rejects_no_colon() {
        assert!(validate_bind_mount("/just/a/path").is_err());
    }

    // --- guess_mime ---

    #[test]
    fn mime_text_files() {
        assert_eq!(guess_mime("file.txt"), "text/plain; charset=utf-8");
        assert_eq!(guess_mime("file.md"), "text/plain; charset=utf-8");
        assert_eq!(guess_mime("file.py"), "text/plain; charset=utf-8");
        assert_eq!(guess_mime("file.rs"), "text/plain; charset=utf-8");
    }

    #[test]
    fn mime_json() {
        assert_eq!(guess_mime("data.json"), "application/json");
    }

    #[test]
    fn mime_images() {
        assert_eq!(guess_mime("photo.png"), "image/png");
        assert_eq!(guess_mime("photo.jpg"), "image/jpeg");
        assert_eq!(guess_mime("photo.jpeg"), "image/jpeg");
    }

    #[test]
    fn mime_unknown_defaults_to_octet_stream() {
        assert_eq!(guess_mime("file.bin"), "application/octet-stream");
        assert_eq!(guess_mime("noext"), "application/octet-stream");
    }

    #[test]
    fn mime_pdf() {
        assert_eq!(guess_mime("doc.pdf"), "application/pdf");
    }
}

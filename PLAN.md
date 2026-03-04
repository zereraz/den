# den v1 — Implementation Plan

## Status: All code written, compiles clean. 32 unit tests pass. Needs Docker for integration test + smoke test.

## Context
Self-hosted Docker-based sandbox runtime for AI agents. Runs on Hetzner cloud VM (no KVM). Rust single binary. Pre-warms container pool for instant sandbox creation. REST + WebSocket API.

Project: `~/Code/Zereraz/den/`

## Architecture

### Agent ↔ Sandbox Model
The agent (LLM) runs **outside** containers. Containers are just "hands" — exec environments. The orchestration harness (pi-mono/nama) translates tool calls into den API calls. This means:
- Containers are extremely lightweight (~5-10MB idle, spike only during exec)
- One LLM can drive many sandboxes concurrently
- The agent "thinks" it has a filesystem/terminal, but it's all remote API calls

### Integration with pi-mono/nama
- Pi-mono has pluggable operations: `BashOperations { exec }`, `ReadOperations { readFile }`, `WriteOperations`, `EditOperations`
- **SandboxProvider** interface: `{ create, destroy }` → returns `SandboxSession` with operations implementations
- **DenProvider**: one implementation that maps operations → den REST/WS calls
- Sandbox is a **nama extension** (not recipes) — one tool (`sandbox_exec`), the LLM decides what to do
- Den is never mentioned in pi-mono/nama code, just one provider implementation

### Industry Research (e2b, Daytona)
- E2B envd: REST for files, gRPC/Connect for processes, 64KB chunks
- Daytona toolbox: Go daemon on port 2280, rich file API, PTY WebSocket
- Both use tar/multipart for file transfer, neither uses `docker exec cat`
- **Consensus**: bytes at wire, strings in SDK. Dedicated file endpoints. Streaming for processes.
- Den follows this pattern: Docker archive API (tar) for files, no in-container daemon needed

## Key Decisions
- **Auto-start**: `POST /sandboxes` claims from pool + starts in one call
- **Exec stdin**: Bidirectional WS (stdin + stdout/stderr streaming)
- **Pool empty**: Fail fast with 503, no blocking wait
- **Container CMD**: `sleep infinity` — stays alive for docker exec
- **Pool containers**: Pre-created as stopped, started on claim. Each gets a named volume.
- **State**: DashMap<String, Sandbox> shared across API/reaper/pool
- **Shutdown**: watch::channel, all background tasks observe it
- **Container names**: `den-{tier}-{uuid_short}`
- **Sandbox IDs**: `{tier}-{uuid_v4}`
- **Volume names**: `den-vol-{uuid_short}`
- **Network**: Default bridge for v1, optional `den-net` in config
- **Storage**: Docker named volumes mounted at `/home/sandbox`
  - Volume survives container death — enables resume
  - Reaper stops containers but keeps volumes (separate 24h TTL)
  - Resume = new container mounting existing volume
- **File transfer**: Docker archive API (tar wrapping), no in-container daemon
- **Security**: Path traversal prevention, SSRF-safe proxy, exec timeout cap, body size limit
- **Single tier**: `default` — 1GB RAM, 1 CPU, 256 PIDs, 24h timeout, pool of 2
- **Base image**: Ubuntu 24.04, Python 3 + uv, Node.js 22 LTS, ripgrep, fd-find, build-essential, sudo

## Volume Lifecycle (no leaks)

| Action | Container | Volume | DashMap |
|---|---|---|---|
| warm_up / replenish | created (stopped) | created + mounted | — |
| `POST /sandboxes` (claim) | started | mounted | inserted (Running) |
| `DELETE /sandboxes/{id}` (release) | stopped + removed | **kept** | **kept** (Completed) |
| `POST /sandboxes/{id}/resume` | new created + started | remounted | updated (Running) |
| `DELETE /sandboxes/{id}/volume` (destroy) | removed | **removed** | **removed** |
| Reaper (expired/idle) | stopped + removed | **kept** | **kept** (Timeout) |
| Reaper (volume TTL 24h) | removed | **removed** | **removed** |

## API

```
POST   /api/v1/sandboxes                    → claim + start
GET    /api/v1/sandboxes                    → list all
GET    /api/v1/sandboxes/{id}               → get one
DELETE /api/v1/sandboxes/{id}               → soft delete (stop, keep volume)
POST   /api/v1/sandboxes/{id}/resume        → resume (new container, existing volume)
DELETE /api/v1/sandboxes/{id}/volume        → hard delete (remove everything)
POST   /api/v1/sandboxes/{id}/exec          → REST exec (300s max timeout)
GET    /api/v1/sandboxes/{id}/exec/ws       → WS exec (bidirectional, 64KB msg, 1h max)
GET    /api/v1/sandboxes/{id}/files/*path   → download file (path-safe, MIME detected)
PUT    /api/v1/sandboxes/{id}/files/*path   → upload file (path-safe, 100MB max)
GET    /proxy/{id}/{port}/*rest             → reverse proxy (SSRF-protected)
GET    /api/v1/resources                    → resource snapshot
GET    /api/v1/health                       → health (pings Docker, 2s timeout)
```

## Files

### `Cargo.toml`
bollard 0.19, axum 0.8 (ws), tokio (full), dashmap 6, serde/toml, clap 4, tracing, uuid, chrono, thiserror, anyhow, tower-http (cors, trace), hyper/hyper-util (proxy), tokio-stream, futures-util, tar, bytes. Edition 2024.

### `src/error.rs` (59 lines)
`DenError` enum with `impl IntoResponse` → HTTP status codes.

### `src/config.rs` (~180 lines)
`DenConfig`, `TierConfig`, `ReaperConfig` with `volume_ttl_secs`. 3 unit tests.

### `src/sandbox.rs` (~180 lines)
State machine: Pending → Starting → Running → Completed/Failed/Timeout. 7 unit tests.

### `src/docker.rs` (~355 lines)
bollard wrapper: volumes, containers, `exec_collect` (REST), `exec_stream` (WS), `download_file`/`upload_file` (tar-based), `get_container_ip`, `container_is_running`.

### `src/pool.rs` (~230 lines)
Warm pool with `PoolEntry { container_id, volume_name }`. claim/release/resume/destroy lifecycle.

### `src/scheduler.rs` (71 lines)
`ResourceSnapshot` from docker system info.

### `src/reaper.rs` (~110 lines)
Phase 1: stop expired/idle/crashed. Phase 2: full cleanup past volume TTL.

### `src/api.rs` (~610 lines)
All REST/WS handlers. `sanitize_file_path()` for path traversal prevention. `guess_mime()`. 100MB body limit. 22 unit tests.

### `src/proxy.rs` (~140 lines)
Reverse proxy with `is_safe_proxy_target()` SSRF protection. 7 unit tests.

### `src/main.rs` (133 lines)
CLI, tracing, config, warm_up, spawn reaper + replenisher, axum::serve with graceful_shutdown.

### `Dockerfile.sandbox`
Ubuntu 24.04, build-essential, git, Python 3 + uv, Node.js 22 LTS, ripgrep, fd-find, sudo. CMD `sleep infinity`.

### `den.toml`
Single `default` tier: 1GB/1CPU/256PIDs/24h timeout/pool=2. Reaper 10s, 300s idle timeout. Volume TTL 24h.

## Security (implemented)
- **Path traversal**: `sanitize_file_path()` resolves components, blocks escape from `/home/sandbox`
- **SSRF proxy**: `is_safe_proxy_target()` allows only Docker bridge IPs (172.16-31.x.x, 192.168.x.x)
- **Exec timeout**: Capped at 300s max regardless of client request
- **Body size**: 100MB limit via axum `DefaultBodyLimit`
- **Health**: Docker ping with 2s timeout (no hang)
- **WS exec**: 64KB message size limit, 1h max session

## Tests (32 passing)
- config: 3 (parse valid, reject zero memory, reject no tiers)
- sandbox: 7 (state transitions, touch, is_expired)
- api/sanitize_file_path: 10 (traversal blocking, valid paths, edge cases)
- api/guess_mime: 5 (text, json, images, pdf, unknown)
- proxy/is_safe_proxy_target: 7 (docker bridge, loopback, link-local, public, ipv6, invalid)

## TODO
- [ ] Build Dockerfile.sandbox on Hetzner, smoke test full flow
- [ ] Wire `scheduler.can_schedule()` into claim path
- [ ] API key auth middleware (config has `api_key` field ready)
- [ ] Orphan cleanup on startup (find `den.managed` volumes/containers not in DashMap)
- [ ] Build nama extension (SandboxProvider → DenProvider)
- [ ] Integration tests with OrbStack/Docker
- [ ] Commit to git

# den

Docker-based sandbox runtime for AI agents. Pre-warms a container pool for instant sandbox creation. REST + WebSocket API.

## Quick Start

```bash
# Build the sandbox image
docker build -t den-sandbox:latest -f Dockerfile.sandbox .

# Run
cargo run -- --config den.toml
```

## API

```
POST   /api/v1/sandboxes                    → create sandbox (spot or dedicated with bind_mounts)
GET    /api/v1/sandboxes                    → list all
GET    /api/v1/sandboxes/{id}               → get one
DELETE /api/v1/sandboxes/{id}               → release (keep volume)
POST   /api/v1/sandboxes/{id}/resume        → resume (new container, same volume + bind mounts)
DELETE /api/v1/sandboxes/{id}/volume        → destroy (remove everything)
POST   /api/v1/sandboxes/{id}/exec          → execute command
GET    /api/v1/sandboxes/{id}/exec/ws       → streaming exec (WebSocket)
GET    /api/v1/sandboxes/{id}/diagnostics   → container resource usage (memory, CPU, PIDs)
GET    /api/v1/sandboxes/{id}/files/{*path}  → download file
PUT    /api/v1/sandboxes/{id}/files/{*path}  → upload file
GET    /proxy/{id}/{port}/{*rest}            → reverse proxy to container
GET    /api/v1/resources                    → resource snapshot
GET    /api/v1/health                       → health check
```

## Usage

```bash
# Create a sandbox (spot — claimed from pre-warmed pool)
curl -X POST localhost:8080/api/v1/sandboxes -H 'Content-Type: application/json' -d '{"tier":"default"}'

# Create a dedicated sandbox with host bind mounts (bypasses pool)
curl -X POST localhost:8080/api/v1/sandboxes -H 'Content-Type: application/json' \
  -d '{"tier":"default","bind_mounts":["/path/to/project:/workspace:rw"]}'

# Execute a command
curl -X POST localhost:8080/api/v1/sandboxes/$ID/exec -H 'Content-Type: application/json' -d '{"cmd":["echo","hello"]}'

# Upload a file
curl -X PUT localhost:8080/api/v1/sandboxes/$ID/files/script.py -d 'print("hello")'

# Download a file
curl localhost:8080/api/v1/sandboxes/$ID/files/script.py

# Container diagnostics (memory, CPU, PIDs)
curl localhost:8080/api/v1/sandboxes/$ID/diagnostics

# Release (stops container, keeps volume for resume)
curl -X DELETE localhost:8080/api/v1/sandboxes/$ID

# Resume (new container, same volume + same bind mounts)
curl -X POST localhost:8080/api/v1/sandboxes/$ID/resume

# Destroy (removes everything)
curl -X DELETE localhost:8080/api/v1/sandboxes/$ID/volume
```

## Configuration

All settings in `den.toml`:

```toml
[server]
host = "0.0.0.0"
port = 8080
# api_key = "secret-token"  # optional bearer token auth

[docker]
# socket = "/var/run/docker.sock"  # default: auto-detect
# network = "den-net"              # default: bridge

[pool]
replenish_interval_secs = 5  # how often to top up warm pool

[reaper]
interval_secs = 10         # how often to check for expired sandboxes
idle_timeout_secs = 300    # stop container after 5min idle
# volume_ttl_secs = 86400 # destroy volume 24h after stop (default)

[tiers.default]
image = "den-sandbox:latest"
memory_mb = 1024           # container memory limit
cpus = 1.0                 # CPU quota
pids = 256                 # max processes
timeout_secs = 86400       # max sandbox lifetime
pool_size = 2              # warm containers ready to claim
readonly_rootfs = false
max_concurrent_execs = 4   # max parallel exec per sandbox (429 when exceeded)

[tiers.default.tmpfs]
"/tmp" = "size=200M,mode=1777"
```

Define multiple tiers (e.g. `tiers.light`, `tiers.heavy`) with different resource limits. Request a tier by name when creating a sandbox.

## Sandbox Image

`Dockerfile.sandbox` builds an Ubuntu 24.04 image with:
- Python 3.12 + uv
- Node.js 22 LTS + pnpm/yarn
- ripgrep, fd-find, jq
- build-essential, cmake
- git, curl, wget

Runs as non-root `sandbox` user (uid 1000) with passwordless sudo. Working directory is `/home/sandbox`, backed by a persistent Docker volume.

## How It Works

### Spot mode (default)

Containers are pre-created in a warm pool (stopped). When you create a sandbox, den claims one from the pool and starts it instantly. Each container gets a named volume at `/home/sandbox` that survives container restarts — release a sandbox, resume it later, and all files are still there.

### Dedicated mode (bind mounts)

Pass `bind_mounts` when creating a sandbox to mount host directories into the container. Dedicated sandboxes bypass the pre-warmed pool — the container is created on-demand with the requested mounts.

```bash
curl -X POST localhost:8080/api/v1/sandboxes -H 'Content-Type: application/json' \
  -d '{"tier":"default","bind_mounts":["/home/user/project:/workspace:rw"]}'
```

Bind mounts give zero-copy filesystem access — the container reads and writes directly to the host. The volume at `/home/sandbox` is still created for caches, tools, and shell state.

Bind mounts are stored on the sandbox, so **resume recreates the container with the same mounts**. Files added to the host directory while the sandbox was stopped are visible immediately after resume.

Format: `host_path:container_path[:mode]` where mode is `ro` or `rw` (default: Docker's default, typically `rw`).

Blocked host paths: `/proc`, `/sys`, `/dev`, `/etc`, `/root`, `/var/run/docker*`. Container path `/home/sandbox` is reserved for the volume.

### Defunct detection

If a container dies unexpectedly (OOM, crash), the reaper marks the sandbox as `defunct`. Exec requests return `503` with a clear message:

```json
{"error": "sandbox <id> is defunct (container died), resume to recover"}
```

Resume a defunct sandbox to get a fresh container with the same volume and bind mounts. Defunct sandboxes are not terminal — they stay resumable until volume TTL expires.

### Exec backpressure

Each sandbox has a concurrent exec limit (`max_concurrent_execs`, default 4 per tier). When all slots are in use, additional exec requests get `429 Too Many Requests`:

```json
{"error": "sandbox <id> exec limit reached (4 concurrent)"}
```

WebSocket exec sessions hold a permit for their entire lifetime. Permits are released automatically when the exec completes or the connection drops.

### Reaper

A background reaper stops idle containers (default 5min) and cleans up volumes past their TTL (default 24h). Crashed containers are transitioned to `defunct` rather than `failed`, keeping them resumable.

## Security

- File access restricted to `/home/sandbox` (path traversal blocked)
- Reverse proxy limited to Docker bridge IPs (SSRF protection)
- Exec timeout capped at 300s
- Request body limit 100MB
- Container resource limits (memory, CPU, PIDs) per tier

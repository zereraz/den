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
POST   /api/v1/sandboxes                    → create sandbox
GET    /api/v1/sandboxes                    → list all
GET    /api/v1/sandboxes/{id}               → get one
DELETE /api/v1/sandboxes/{id}               → release (keep volume)
POST   /api/v1/sandboxes/{id}/resume        → resume (new container, same volume)
DELETE /api/v1/sandboxes/{id}/volume        → destroy (remove everything)
POST   /api/v1/sandboxes/{id}/exec          → execute command
GET    /api/v1/sandboxes/{id}/exec/ws       → streaming exec (WebSocket)
GET    /api/v1/sandboxes/{id}/files/{*path}  → download file
PUT    /api/v1/sandboxes/{id}/files/{*path}  → upload file
GET    /proxy/{id}/{port}/{*rest}            → reverse proxy to container
GET    /api/v1/resources                    → resource snapshot
GET    /api/v1/health                       → health check
```

## Usage

```bash
# Create a sandbox
curl -X POST localhost:8080/api/v1/sandboxes -H 'Content-Type: application/json' -d '{"tier":"default"}'

# Execute a command
curl -X POST localhost:8080/api/v1/sandboxes/$ID/exec -H 'Content-Type: application/json' -d '{"cmd":["echo","hello"]}'

# Upload a file
curl -X PUT localhost:8080/api/v1/sandboxes/$ID/files/script.py -d 'print("hello")'

# Download a file
curl localhost:8080/api/v1/sandboxes/$ID/files/script.py

# Release (stops container, keeps volume for resume)
curl -X DELETE localhost:8080/api/v1/sandboxes/$ID

# Resume (new container, same volume — files persist)
curl -X POST localhost:8080/api/v1/sandboxes/$ID/resume

# Destroy (removes everything)
curl -X DELETE localhost:8080/api/v1/sandboxes/$ID/volume
```

## Configuration

See `den.toml` for tier definitions, pool size, reaper settings, and volume TTL.

## Sandbox Image

`Dockerfile.sandbox` builds an Ubuntu 24.04 image with Python 3 + uv, Node.js 22 LTS, ripgrep, fd-find, and build-essential. Runs as non-root `sandbox` user with sudo.

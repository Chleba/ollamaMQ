# ollamaMQ

`ollamaMQ` is a high-performance, asynchronous message queue dispatcher and load balancer designed to sit in front of one or more [Ollama](https://ollama.ai/) or [LM Studio](https://lmstudio.ai/) API instances. It acts as a smart proxy that queues incoming requests from multiple users and dispatches them in parallel to multiple backends using a fair-share round-robin scheduler with least-connections load balancing.

![Rust](https://img.shields.io/badge/rust-2024-orange.svg)
![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Ollama](https://img.shields.io/badge/Ollama-Proxy-7ed321.svg)

## 🚀 Features

- **Multi-Backend Load Balancing**: Distribute requests across multiple Ollama or LM Studio instances using a **Least Connections + Round Robin** strategy. Automatically detects backend API type (Ollama `/api/*` vs OpenAI `/v1/*`) and routes each request to a compatible backend.
- **Model-Aware Routing**: Automatically identifies the requested model from the request body and routes the request only to backends that have that specific model loaded. This prevents 404 errors when different models are distributed across multiple backends.
- **Smart Model Matching**: Robust matching that handles common variations like `:latest` tags and case-insensitivity. For example, a request for `llama3` will correctly match `llama3:latest` on the backend.
- **Model Control**: Load and unload models on connected backends (Ollama and LM Studio 0.3.6+) directly from the TUI (`L`/`U`) or the admin HTTP API — without touching the backend servers themselves.
- **Parallel Processing**: Unlike basic proxies, `ollamaMQ` can process multiple requests simultaneously (one per available backend), significantly increasing throughput for multiple users.
- **Backend Health Checks**: Automatically monitors backend status every 10 seconds. Probes for both API type (Ollama vs OpenAI) and the list of currently available models (via `/api/tags` and `/v1/models`). Offline instances are temporarily skipped and marked in the TUI.
- **Per-User Queuing**: Each user (identified by the `X-User-ID` header) has their own FIFO queue.
- **Fair-Share Scheduling**: Prevents any single user from monopolizing all available backends.
- **Transparent Header Forwarding**: Full support for all HTTP headers (including `X-User-ID`) passed to and from the backend, ensuring compatibility with tools like **Claude Code**.
- **VIP & Boost Modes**: Absolute priority (VIP) or increased frequency (Boost) for specific users.
- **Real-Time TUI Dashboard**: Monitor backend health, active requests, queue depths, and throughput in real-time.
- **OpenAI Compatibility**: Supports standard OpenAI-compatible endpoints.
- **Async Architecture**: Built on `tokio` and `axum` for high concurrency.

![ollamaMQ TUI Dashboard](demo.gif)

## 🛠️ Installation

Ensure you have [Rust](https://rustup.rs/) (2024 edition or later) and [Ollama](https://ollama.ai/) installed.

### Option 1: Install via Cargo (Recommended)

```bash
cargo install ollamaMQ
```

### Option 2: From Source

1. Clone the repository:

   ```bash
   git clone https://github.com/Chleba/ollamaMQ.git
   cd ollamaMQ
   ```

2. Build and install locally:
   ```bash
   cargo install --path .
   ```

## 🏃 Usage

### Docker Installation

#### Using Docker Compose (Recommended)

1. Ensure Docker and Docker Compose are installed.
2. Start your local Ollama instance (defaulting to `localhost:11434`).
3. Run:
   ```bash
   docker compose up -d
   ```

#### Using Docker CLI

First build the image from the local Dockerfile:

```bash
docker build -t chlebon/ollamamq .
```

Then run the container:

```bash
docker run -d \
  --name ollamamq \
  -p 11435:11435 \
  --restart unless-stopped \
  chlebon/ollamamq
```

### Command Line Arguments

`ollamaMQ` supports several options to configure the proxy:

- `-p, --port <PORT>`: Port to listen on (default: `11435`)
- `-H, --host <HOST>`: Host/interface to bind to (default: `127.0.0.1`, loopback only). Set `--host 0.0.0.0` to allow LAN/Docker access.
- `-o, --backend-urls <URL1,URL2>`: Comma-separated list of backend server URLs (Ollama, LM Studio, etc.) (default: `http://localhost:11434`)
- `-t, --timeout <SECONDS>`: Request timeout in seconds (default: `300`)
- `--no-tui`: Disable the interactive TUI dashboard (useful for Docker/CI). In this mode, logs are written verbosely to **stderr** (default level `debug`) so they can be captured by a service manager's journal, Docker, or piped to a file.
- `--load-keep-alive <SECONDS>`: How long a model stays loaded after a model-control "load" (sent as Ollama's `keep_alive`; LM Studio loads are unaffected beyond its own TTL). Ollama's own default is only 5 minutes, so the default here is `86400` (24 h).
- `--allow-all-routes`: Enable fallback proxy for non-standard endpoints
- `-h, --help`: Print help message
- `-V, --version`: Print version information

**Example:**

```bash
ollamaMQ --port 8080 --ollama-urls http://10.0.0.1:11434,http://10.0.0.2:11434 --timeout 600
```

**Docker Example:**

```bash
docker run -d \
  --name ollamamq \
  -p 8080:8080 \
  chlebon/ollamamq --port 8080 --ollama-urls http://192.168.1.5:11434 --timeout 600
```

### API Proxying

Point your LLM clients to the `ollamaMQ` port (`11435`) and include the `X-User-ID` header.

#### Supported Endpoints:

- `GET /health` (Internal health check)
- `GET /` (Backend Status)
- `POST /api/generate`
- `POST /api/chat`
- `POST /api/embed`
- `POST /api/embeddings`
- `GET /api/tags`
- `POST /api/show`
- `POST /api/create`
- `POST /api/copy`
- `DELETE /api/delete`
- `POST /api/pull`
- `POST /api/push`
- `GET/HEAD/POST /api/blobs/{digest}`
- `GET /api/ps`
- `GET /api/version`
- `POST /v1/chat/completions` (OpenAI Compatible)
- `POST /v1/completions` (OpenAI Compatible)
- `POST /v1/embeddings` (OpenAI Compatible)
- `GET /v1/models` (OpenAI Compatible)
- `GET /v1/models/{model}` (OpenAI Compatible)


#### Example (cURL):

```bash
curl -X POST http://localhost:11435/api/chat \
  -H "X-User-ID: developer-1" \
  -d '{
    "model": "qwen3.5:35b",
    "messages": [{"role": "user", "content": "Explain quantum computing."}],
    "stream": true
  }'
```

### Model Control (Admin API)

Models can be loaded and unloaded on connected backends without touching the backend servers. Supported backends:

- **Ollama** — no dedicated endpoint exists; loading is a `POST /api/generate` with an empty prompt (the scheduler loads the model and returns immediately), unloading is the same call with `keep_alive: 0`.
- **LM Studio 0.3.6+** — the native `POST /api/v1/models/load` / `POST /api/v1/models/unload` endpoints (unload targets a specific loaded instance). Older LM Studio versions are detected and a friendly error points at the `lms load`/`lms unload` CLI.
- Plain OpenAI backends (vLLM, etc.) have no control API and are rejected with a clear error.

All three endpoints are served by the proxy itself (they are never proxied to a backend) and are protected by the same optional `OLLAMA_MQ_API_KEY` auth as the proxy routes.

#### `GET /admin/models`

Per-backend inventory: index, URL, online status, detected API type, LM Studio flag, active request count, available models, currently loaded models, and any in-flight control operation.

```bash
curl -s http://localhost:11435/admin/models | python3 -m json.tool
```

#### `POST /admin/models/load` and `POST /admin/models/unload`

Body:

```json
{ "backend": 0, "model": "llama3" }
```

`backend` accepts:

- a **numeric index** (0-based, in the order of `--backend-urls`),
- a **URL** (exact or substring match), or
- `"any"` — the proxy picks a suitable online, idle backend (for load: the first backend where the model resolves; for unload: the first backend that actually has it loaded).

`model` uses the same smart matching as request routing: exact match, then `:latest`/case-insensitive, then a *unique* substring. Ambiguous names are rejected — the proxy never guesses.

Responses:

| Status | Meaning |
| ------ | ------- |
| `202`  | Accepted — the operation started in the background (response body contains the canonical model name and the backend URL). |
| `400`  | Bad request — empty model, backend offline, unsupported backend type, or unloading a model that is not loaded. |
| `404`  | Unknown backend, model not found on the backend, or no suitable backend for `"any"`. |
| `409`  | Conflict — another control operation is already in flight on that backend, or the backend is busy with active requests. |

```bash
# Load (llama3 resolves to llama3:latest)
curl -s -X POST http://localhost:11435/admin/models/load \
  -H "Content-Type: application/json" \
  -d '{"backend": 0, "model": "llama3"}'

# Unload by URL (frees GPU memory on Ollama)
curl -s -X POST http://localhost:11435/admin/models/unload \
  -H "Content-Type: application/json" \
  -d '{"backend": "http://10.0.0.2:11434", "model": "qwen2.5:7b"}'

# Let the proxy pick a backend
curl -s -X POST http://localhost:11435/admin/models/load \
  -H "Content-Type: application/json" \
  -d '{"backend": "any", "model": "llama3:latest"}'

# With API-key auth enabled
curl -s http://localhost:11435/admin/models -H "Authorization: Bearer supersecret"
```

**Note:** Ollama keeps a model loaded for `keep_alive` seconds only (its own default is 5 minutes). A control "load" sends the `--load-keep-alive` value (default 24 h) so the model actually stays resident; an "unload" sends `keep_alive: 0`.

### Dashboard Controls

The interactive TUI dashboard provides a live view of the dispatcher's state:

- **`j` / `k`** or **Arrows**: Navigate the selected list (Users, Backends, or Blocked Items).
- **`Tab`** or **`h` / `l`**: Switch between the **Backends**, **Users**, and **Blocked** panels.
- **`Space`** or **`Enter`**: Expand/collapse the available models list for the selected backend (in the Backends panel).
- **`L`**: Load a model on the selected backend (Backends panel) — type the model name, confirm with `Enter`, cancel with `Esc`.
- **`U`**: Unload a model from the selected backend (Backends panel).
- **`p`**: Toggle **VIP** status for the selected user (absolute priority).
- **`b`**: Toggle **Boost** status for the selected user (prioritizes every 2nd request).
- **`x`**: Block the selected user.
- **`X`**: Block the selected user's IP address.
- **`u`**: Unblock the selected user or IP (works in both panels).
- **`q`** or **Esc**: Exit the dashboard and stop the application.
- **`?`**: Toggle detailed help overlay.

**Visual Indicators:**
- `▶` / `▼`: Indicates if a backend's model list is collapsed or expanded.
- `★` (Magenta): **VIP User** (absolute priority).
- `⚡` (Yellow): **Boosted User** (every 2nd request priority).
- `▶` (Cyan): Request is currently being processed/streamed.
- `●` (Green): Backend is Online or User has requests waiting in the queue.
- `○` (Gray): User is idle or Backend is Offline.
- `✖` (Red): User or IP is blocked.
- `⟳` (Cyan): A model load/unload control operation is in progress on that backend (recent results flash `✓`/`✖` for ~10 s).

### Logging

`ollamaMQ` uses [`tracing`](https://docs.rs/tracing) and behaves differently depending on the mode:

- **TUI mode** (default, interactive terminal): Logs are written to the `ollamamq.log` file in the current working directory. This keeps the terminal clear for the dashboard. Default level is `info`.
- **`--no-tui` mode** (Docker/CI/service): Logs are written to **stderr** at a default level of `debug`, so you can see everything — backend health checks, per-request routing, backend detection, and errors. This is ideal for capturing output via the systemd journal, Docker, or piping to a file.

Override the level at any time with the standard `RUST_LOG` environment variable:

```bash
# Most verbose (all debug detail)
RUST_LOG=debug ollamaMQ --no-tui

# Quieter (info + errors only)
RUST_LOG=info ollamaMQ --no-tui
```

### Running as a systemd Service

To run `ollamaMQ` as a background service with full log visibility in the journal:

1. Copy the provided service file to your system's unit directory:

   ```bash
   sudo cp ollamamq.service /etc/systemd/system/
   ```

2. Edit the file and set the correct `ExecStart` path to your installed binary (e.g. `~/.cargo/bin/ollamaMQ`) and your backend URLs:

   ```ini
   ExecStart=/usr/local/bin/ollamaMQ --no-tui --port 11435 --backend-urls http://localhost:11434
   ```

   > **Note:** If you installed `ollamaMQ` with `cargo install`, the binary is at `~/.cargo/bin/ollamaMQ`, **not** `/usr/local/bin/ollamaMQ`. Point `ExecStart` at the real path, or create a symlink so the default path works:
   >
   > ```bash
   > # Find the real path
   > which ollamaMQ
   >
   > # Option A: edit ExecStart to use the real path
   > # Option B: symlink it to the expected location
   > sudo ln -s "$HOME/.cargo/bin/ollamaMQ" /usr/local/bin/ollamaMQ
   > ```

3. Reload systemd, then enable and start the service:

   ```bash
   sudo systemctl daemon-reload
   sudo systemctl enable --now ollamamq
   ```

4. Follow the logs in real time via the journal:

   ```bash
   journalctl -u ollamamq -f
   ```

   Or show the most recent 100 lines:

   ```bash
   journalctl -u ollamamq -n 100
   ```

Because `--no-tui` defaults to `debug` logging on stderr, **all** dispatcher events (backend health, request routing, errors) are captured in the journal — no separate log file needed.

## 🐳 Docker

### Docker Compose

The included `docker-compose.yml` provides a ready-to-use configuration:

```yaml
services:
  ollamamq:
    build: .
    image: chlebon/ollamamq:latest
    container_name: ollamamq
    ports:
      - "11435:11435"
    environment:
      - OLLAMA_URLS=http://host.docker.internal:11434
      - PORT=11435
    extra_hosts:
      - "host.docker.internal:host-gateway"
    restart: unless-stopped
```

**Note for Linux Users:**
When running in Docker on Linux to access a host-based Ollama:

1.  **Listen on all interfaces:** Ollama must be configured to listen on `0.0.0.0`. You can do this by setting `export OLLAMA_HOST=0.0.0.0` before starting the Ollama service (or editing the systemd unit file).
2.  **Firewall:** Ensure your firewall (e.g., `ufw`) allows traffic from the Docker bridge (usually `172.17.0.1/16`) to port `11434`.
3.  **Host Gateway:** The `extra_hosts` setting in `docker-compose.yml` maps `host.docker.internal` to your host's IP address.

### Dockerfile

The Dockerfile uses a multi-stage build:

- **Build stage**: Uses `rust:1.85-alpine` to compile the release binary
- **Runtime stage**: Uses `alpine:3.20` with only `ca-certificates` for a minimal footprint (~10MB)

### Environment Variables

| Variable      | Description                    | Default                  |
| ------------- | ------------------------------ | ------------------------ |
| `OLLAMA_URLS` | URLs of the Ollama servers     | `http://localhost:11434` |
| `PORT`        | Port for ollamaMQ to listen on | `11435`                  |
| `HOST`        | Host/interface to bind to      | `0.0.0.0`                |
| `TIMEOUT`     | Request timeout in seconds     | `300`                    |
| `OLLAMA_MQ_API_KEY` | Optional API key; enables auth when set | *(unset — auth disabled)* |

### 🔒 Security (Optional API-Key Auth)

By default, `ollamaMQ` only listens on loopback (`127.0.0.1`), so it is only reachable from the local machine. If you expose it to the network with `--host 0.0.0.0`, set the `OLLAMA_MQ_API_KEY` environment variable to a secret key to protect the proxy. Auth is **opt-in**: if the variable is unset or empty, behavior is unchanged and no key is required.

When a key is set, every request **except `/health`** must present it via one of two headers:

- `Authorization: Bearer <key>`
- `X-API-Key: <key>`

Requests with a missing or wrong key get `401 Unauthorized` (`{"error":"unauthorized"}`). The `/health` endpoint always stays unauthenticated so health checks and Docker healthchecks keep working.

```bash
# Enable auth
OLLAMA_MQ_API_KEY=supersecret ollamaMQ --no-tui

# Call the API with the key (either header works)
curl -X POST http://localhost:11435/api/chat \
  -H "Authorization: Bearer supersecret" \
  -H "X-User-ID: developer-1" \
  -d '{"model": "llama3", "messages": [{"role": "user", "content": "Hi"}]}'

# ...or with X-API-Key
curl http://localhost:11435/api/tags -H "X-API-Key: supersecret"

# /health never requires a key
curl http://localhost:11435/health
```

In Docker, just pass the variable through — the binary reads it directly from the environment:

```bash
docker run -d --name ollamamq -p 11435:11435 \
  -e OLLAMA_MQ_API_KEY=supersecret \
  chlebon/ollamamq
```

### Connecting to Different Ollama Servers

#### Local Ollama (on host machine)

```bash
docker run -d \
  --name ollamamq \
  -p 11435:11435 \
  -e OLLAMA_URLS=http://host.docker.internal:11434 \
  chlebon/ollamamq
```

#### Remote Ollama Server

```bash
docker run -d \
  --name ollamamq \
  -p 11435:11435 \
  -e OLLAMA_URLS=https://ollama.example.com:11434 \
  chlebon/ollamamq
```

#### Custom Port on Same Server

```bash
docker run -d \
  --name ollamamq \
  -p 8080:8080 \
  -e OLLAMA_URLS=http://host.docker.internal:11436 \
  -e PORT=8080 \
  chlebon/ollamamq
```

#### Ollama in Docker (different container)

```bash
docker run -d \
  --name ollamamq \
  --network ollama-network \
  -p 11435:11435 \
  -e OLLAMA_URLS=http://ollama:11434 \
  chlebon/ollamamq
```

### Port Configuration

- **11435**: The proxy port that clients connect to (exposed by default)
- **11434**: The Ollama server port (internal, not exposed)

To change the proxy port, use the `PORT` environment variable:

```bash
docker run -d \
  --name ollamamq \
  -p 8080:8080 \
  -e PORT=8080 \
  chlebon/ollamamq
```

## 🏗️ Architecture

- **`src/main.rs`**: Entry point, HTTP server initialization, and TUI lifecycle management.
- **`src/dispatcher.rs`**: Core logic for queuing, round-robin scheduling, and Ollama proxying.
- **`src/tui.rs`**: Implementation of the terminal-based monitoring dashboard.
- **`src/control.rs`**: Model load/unload control — backend probing, model-name resolution, Ollama/LM Studio executors, and the admin HTTP API.

### Request Flow

1. Client sends a request with `X-User-ID`.
2. `ollamaMQ` pushes the request into a user-specific queue.
3. The background worker checks for available backends (Online & not busy).
4. If a backend is free, the worker pops the next task (fair-share rotation) and **spawns a parallel task**.
5. The request is proxied to the selected Ollama backend.
6. The response is streamed back to the client in real-time, while the worker can immediately start another task on a different backend.

## 📦 Publishing to Docker Hub

To publish a new version of `ollamaMQ` to Docker Hub, follow these steps:

1. **Update Version**: Update the version number in `Cargo.toml`.
2. **Build and Tag**:

   ```bash
   # Build the image for the current version
   docker build -t chlebon/ollamamq:v0.2.4 .
   
   # Tag it as latest
   docker tag chlebon/ollamamq:v0.2.4 chlebon/ollamamq:latest
   ```

3. **Push to Hub**:

   ```bash
   # Log in to Docker Hub (if not already logged in)
   docker login
   
   # Push the versioned tag
   docker push chlebon/ollamamq:v0.2.4
   
   # Push the latest tag
   docker push chlebon/ollamamq:latest
   ```

## 🧪 Development

### Stress Testing

You can use the provided `test_dispatcher.sh` script to simulate multiple users and verify the dispatcher's behavior under load:

```bash
./test_dispatcher.sh
```

![ollamaMQ Stress Test](demo-test.gif)

## 📝 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details (if applicable).

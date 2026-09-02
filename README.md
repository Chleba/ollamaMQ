# ollamaMQ

`ollamaMQ` is a high-performance, asynchronous message queue dispatcher and load balancer designed to sit in front of one or more [Ollama](https://ollama.ai/) or [LM Studio](https://lmstudio.ai/) API instances. It acts as a smart proxy that queues incoming requests from multiple users and dispatches them in parallel to multiple backends using a fair-share round-robin scheduler with least-connections load balancing.

![Rust](https://img.shields.io/badge/rust-2024-orange.svg)
![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Ollama](https://img.shields.io/badge/Ollama-Proxy-7ed321.svg)

## 🚀 Features

- **Multi-Backend Load Balancing**: Distribute requests across multiple Ollama or LM Studio instances using a **Least Connections + Round Robin** strategy. Automatically detects backend API type (Ollama `/api/*` vs OpenAI `/v1/*`) and routes each request only to a compatible backend — even when a specific model is requested, so an Ollama-family call never reaches an LM Studio server that merely lists the same model.
- **Model-Aware Routing**: Automatically identifies the requested model from the request body and routes it only to backends that list that model. Among those, selection is load-aware: first to backends where the model is already loaded and ready (Ollama `/api/ps`, LM Studio loaded instances; vLLM-style backends that don't report loaded state count as ready when they list the model), then to cold backends with nothing loaded, and only as a last resort to a backend that would have to evict a different loaded model — so a request for `qwen3.8-27b` goes to the server that has it in GPU memory instead of making a busy server with another model load it. Name matching is deterministic and bounded — exact, `:tag`/case-insensitive, or publisher-/quant-suffixed variants of the same id (`qwen3.8-27b` reaches `unsloth/qwen3.8-27b@q8_0`) — but never arbitrary substrings (so it will not be routed to an unrelated `...-abliterated` build).
- **Smart Model Matching**: Robust matching that handles common variations like `:latest` tags and case-insensitivity. For example, a request for `llama3` will correctly match `llama3:latest` on the backend.
- **Model Control**: Load and unload models on connected backends (Ollama and LM Studio 0.3.6+) directly from the TUI (`L`/`U`) or the admin HTTP API — without touching the backend servers themselves.
- **Parallel Processing**: Unlike basic proxies, `ollamaMQ` can process multiple requests simultaneously (one per available backend), significantly increasing throughput for multiple users.
- **Backend Health Checks**: Automatically monitors backend status every 10 seconds. Probes for both API type (Ollama vs OpenAI) and the list of currently available models (via `/api/tags` and `/v1/models`). Endpoints a backend rejects are remembered and skipped until re-checked, so incompatible backends don't spam warnings. Offline instances are temporarily skipped and marked in the TUI.
- **Fail-Fast for Unsatisfiable Requests**: If no online backend can ever serve a queued request (wrong API family or model absent everywhere), it is answered with `503` after `stuck_timeout` seconds instead of hanging forever. Requests merely waiting for a busy or loading backend are unaffected.
- **Per-Model Concurrency Limits**: Each model entry's `max_concurrent_requests` caps how many in-flight requests one backend may serve for that model, bounded globally by `settings.max_concurrent_per_backend` (default 1 — the historical one-request-per-backend behavior; raise it to let a single backend handle several requests at once).
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
{ "backend": 0, "model": "llama3", "num_ctx": 16384, "keep_alive": 3600, "identifier": "big-ctx" }
```

`backend` accepts:

- a **numeric index** (0-based, in the order of `--backend-urls`),
- a **URL** (exact or substring match), or
- `"any"` — the proxy picks a suitable online, idle backend (for load: the first backend where the model resolves; for unload: the first backend that actually has it loaded).

`model` is resolved against each backend's model list: exact match, then `:latest`/case-insensitive, then a *unique* substring (admin ops only). Ambiguous names are rejected — the proxy never guesses.

Optional fields (load only):

- `num_ctx` — max context window, sent as Ollama `options.num_ctx` or LM Studio `context_length` (omitted = backend default).
- `keep_alive` — overrides `--load-keep-alive` (seconds) for this load. `-1` = keep forever.
- `identifier` — free-form label shown with the operation in the TUI and API responses.

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

## ⚙️ Configuration (`appconf.yaml`)

Everything about your `ollamaMQ` instance lives in one YAML file with three sections: **backends** (what to connect to), **settings** (runtime options) and **models** (which models to load where). The default path is `appconf.yaml` in the working directory; override it with `-c/--model-config`. A missing file is fine — defaults apply.

```yaml
# --- Backends: every Ollama / LM Studio / OpenAI-compatible server you want to use
backends:
  - http://10.137.1.1:11434      # e.g. Ollama
  - http://10.137.1.2:1234       # e.g. LM Studio

# --- Runtime settings (all optional — defaults shown)
settings:
  port: 11435                  # proxy listen port (default 11435)
  host: 127.0.0.1              # bind interface; use 0.0.0.0 for LAN/Docker access (default 127.0.0.1)
  timeout: 300                 # per-request timeout in seconds (default 300)
  load_keep_alive: 86400       # how long model-control loads stay resident; -1 = forever (default 86400)
  allow_all_routes: false      # also proxy non-standard endpoints as fallback (default false)
  stuck_timeout: 60            # fail-fast 503 after N s when no backend can ever serve a queued request (default 60)
  max_concurrent_per_backend: 1  # global cap of in-flight requests per backend; raise to let one backend handle several at once (default 1)

# --- Models to load on startup / reload, via the model-control logic
models:
  - name: "gpt-oss:120b"        # model name as the backend knows it
    identifier: "my-gpt"        # label attached to this entry's load op (TUI / admin API)
    max_ctx: 128000             # context window — Ollama num_ctx / LM Studio context_length
    keep_alive: 86400           # seconds the model stays resident after load (Ollama; -1 = forever)
    max_concurrent_requests: 3  # in-flight requests allowed for this model on one backend (default 1)
    backends:                   # which backends to load it on (exact or substring URL match); omitted/empty = any suitable backend
      - http://10.137.1.1:11434
```

**How it's applied:**

- `backends` and `settings` are read at **startup only** — restart to change them.
- `models` is applied automatically at startup (once backend probes have run) and re-applied any time you press **`r`** in the TUI. Application is additive: each target endpoint is checked live first (Ollama `/api/ps`, LM Studio loaded instances), so models already resident there — e.g. still loaded from an earlier run because of long `keep_alive` — are skipped instead of being loaded twice; everything else gets a load started. It never unloads anything, and loads for the same backend run one at a time (backends reject parallel control ops). Every attempt, including skips, is reported in the TUI Logs panel (`⟳ CTL`) and in the log output.
- Explicit CLI flags override file values when given (e.g. `--backend-urls` over `backends`, `--port`/`--host`/`--timeout` over `settings`).
- A non-empty `models[].backends` list also **pins routing**: requests for that model are only sent to the listed backends (case-insensitive substring URL match, the same rule as load targeting) and never to other suitable ones. If none of the pinned backends is online/eligible, the request fails with HTTP 503 after `stuck_timeout` (error: `no configured backend available for model '<model>'`) instead of being routed elsewhere. If `backends` is empty/omitted, or the model is not in the config at all, the existing behavior applies — any online backend that lists the model may serve it.

See [`appconf.yaml.example`](appconf.yaml.example) for a fully commented template.

## Request logging

Every proxied request (`IN`) and upstream response (`OUT`) is written as JSON lines to `settings.request_log_path` (default `ollamamq-requests.jsonl`). Each line is one JSON object:

| field | meaning |
| --- | --- |
| `ts` | unix time in milliseconds |
| `dir` | `IN` (proxied request) or `OUT` (upstream response) |
| `user` | client address |
| `model` | model name, when resolvable (otherwise null) |
| `backend` | upstream backend URL (may be null) |
| `method` | HTTP method |
| `path` | request path |
| `status` | response status code (responses only) |
| `bytes` | total body size in bytes |
| `content_type` | body content type |
| `content` | body preview, truncated to `log_content_limit` with a `...[truncated: N bytes total]` marker |

**Rotation:** when the file exceeds `request_log_max_bytes` it is renamed to `.0` (existing `.0` → `.1`, etc.), at most `request_log_max_files` rotated files are kept and the oldest is deleted. The current file size is re-read at startup, so sizing/rotation survives restarts.

**TUI:** the bottom "Requests" panel shows these newest-first with a one-line content preview; press **Enter** on a row for the full-content detail view (`j`/`k` or `PgUp`/`PgDn` scroll, `q`/`Esc`/`Enter` close).

> **Note:** request/response headers are deliberately **not** logged — this keeps API keys out of the file.

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
      # URLs of backend servers (Ollama, LM Studio, etc.)
      - BACKEND_URLS=http://host.docker.internal:11434,http://host.docker.internal:1234
      - PORT=11435
      - TIMEOUT=300
      - RUST_LOG=info
    command: ["--no-tui"]
    extra_hosts:
      - "host.docker.internal:host-gateway"
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "wget", "--no-verbose", "--tries=1", "--spider", "http://localhost:11435/health"]
      interval: 30s
      timeout: 10s
      retries: 3
      start_period: 10s
```

**Note for Linux Users:**
When running in Docker on Linux to access a host-based Ollama:

1.  **Listen on all interfaces:** Ollama must be configured to listen on `0.0.0.0`. You can do this by setting `export OLLAMA_HOST=0.0.0.0` before starting the Ollama service (or editing the systemd unit file).
2.  **Firewall:** Ensure your firewall (e.g., `ufw`) allows traffic from the Docker bridge (usually `172.17.0.1/16`) to port `11434`.
3.  **Host Gateway:** The `extra_hosts` setting in `docker-compose.yml` maps `host.docker.internal` to your host's IP address.

### Dockerfile

The Dockerfile uses a multi-stage build:

- **Build stage**: Uses `rust:1.97-alpine` (pinned) to compile the release binary
- **Runtime stage**: Uses `alpine:3.20` with only `ca-certificates` for a minimal footprint (~10MB)

### Environment Variables

| Variable       | Description                                        | Default                  |
| -------------- | -------------------------------------------------- | ------------------------ |
| `BACKEND_URLS` | URLs of the backend servers (Ollama, LM Studio, …) | `http://localhost:11434` |
| `OLLAMA_URLS`  | Legacy alias for `BACKEND_URLS` (used only when `BACKEND_URLS` is unset) | — |
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

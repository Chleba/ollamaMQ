# ollamaMQ

`ollamaMQ` is a high-performance, asynchronous message queue dispatcher designed to sit in front of an [Ollama](https://ollama.ai/) API instance. It acts as a smart proxy that queues incoming requests from multiple users and dispatches them sequentially to the Ollama backend, preventing resource exhaustion and ensuring fair sharing of GPU/CPU resources.

![Rust](https://img.shields.io/badge/rust-2024-orange.svg)
![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Ollama](https://img.shields.io/badge/Ollama-Proxy-7ed321.svg)

## 🚀 Features

- **Per-User Queuing**: Each user (identified by the `X-User-ID` header) has their own FIFO queue.
- **Fair-Share Round-Robin Scheduling**: A background worker rotates through active users, processing one request at a time from each to prevent any single user from monopolizing the backend.
- **Full Streaming Support**: Proxies streaming responses from Ollama in real-time, maintaining per-user ordering while delivering tokens as they are generated.
- **Real-Time TUI Dashboard**: A built-in terminal interface powered by `ratatui` for monitoring queue depths, active users, and request throughput in real-time.
- **OpenAI Compatibility**: Supports standard OpenAI-compatible endpoints, making it easy to use with existing tools and libraries.
- **Async Architecture**: Built on `tokio` and `axum` for non-blocking I/O and high concurrency.

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
- `-o, --ollama-url <URL>`: Ollama server URL (default: `http://localhost:11434`)
- `-t, --timeout <SECONDS>`: Request timeout in seconds (default: `300`)
- `--no-tui`: Disable the interactive TUI dashboard (useful for Docker/CI)
- `-h, --help`: Print help message
- `-V, --version`: Print version information

**Example:**
```bash
ollamaMQ --port 8080 --ollama-url http://192.168.1.5:11434 --timeout 600 --no-tui
```

**Docker Example:**
```bash
docker run -d \
  --name ollamamq \
  -p 8080:8080 \
  chlebon/ollamamq --port 8080 --ollama-url http://192.168.1.5:11434 --timeout 600
```

### API Proxying

Point your LLM clients to the `ollamaMQ` port (`11435`) and include the `X-User-ID` header.

#### Supported Endpoints:
- `GET /health` (Internal health check)
- `POST /api/generate` (Ollama Native)
- `POST /api/chat` (Ollama Native)
- `POST /v1/chat/completions` (OpenAI Compatible)
- `POST /v1/completions` (OpenAI Compatible)

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

### Dashboard Controls

The interactive TUI dashboard provides a live view of the dispatcher's state:

- **`j` / `k`** or **Arrows**: Navigate the user list.
- **`q`** or **Esc**: Exit the dashboard.
- **`h`**: Toggle detailed help.

### Logging

Logs are automatically written to `ollamamq.log` in the current working directory. This keeps the terminal clear for the TUI dashboard while allowing you to monitor system events and debug backend communication.

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
      - OLLAMA_URL=http://host.docker.internal:11434
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

| Variable | Description | Default |
|----------|-------------|--------|
| `OLLAMA_URL` | URL of the Ollama server | `http://localhost:11434` |
| `PORT` | Port for ollamaMQ to listen on | `11435` |
| `TIMEOUT` | Request timeout in seconds | `300` |

### Connecting to Different Ollama Servers

#### Local Ollama (on host machine)
```bash
docker run -d \
  --name ollamamq \
  -p 11435:11435 \
  -e OLLAMA_URL=http://host.docker.internal:11434 \
  chlebon/ollamamq
```

#### Remote Ollama Server
```bash
docker run -d \
  --name ollamamq \
  -p 11435:11435 \
  -e OLLAMA_URL=https://ollama.example.com:11434 \
  chlebon/ollamamq
```

#### Custom Port on Same Server
```bash
docker run -d \
  --name ollamamq \
  -p 8080:8080 \
  -e OLLAMA_URL=http://host.docker.internal:11436 \
  -e PORT=8080 \
  chlebon/ollamamq
```

#### Ollama in Docker (different container)
```bash
docker run -d \
  --name ollamamq \
  --network ollama-network \
  -p 11435:11435 \
  -e OLLAMA_URL=http://ollama:11434 \
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

### Request Flow
1. Client sends a request to one of the supported endpoints with `X-User-ID`.
2. `ollamaMQ` pushes the request into a user-specific queue.
3. The background worker selects the next user in rotation and pops a task.
4. The task is forwarded to the Ollama backend (`localhost:11434`).
5. The response is streamed back to the client in real-time through an async channel, keeping the worker occupied until the generation is complete.

## 🧪 Development

### Stress Testing

You can use the provided `test_dispatcher.sh` script to simulate multiple users and verify the dispatcher's behavior under load:

```bash
./test_dispatcher.sh
```

![ollamaMQ Stress Test](demo-test.gif)

## 📝 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details (if applicable).

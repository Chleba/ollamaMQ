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

![ollamaMQ TUI Dashboard](demo.png)

## 🛠️ Installation

Ensure you have [Rust](https://rustup.rs/) (2024 edition or later) and [Ollama](https://ollama.ai/) installed.

1. Clone the repository:
   ```bash
   git clone https://github.com/yourusername/ollamaMQ.git
   cd ollamaMQ
   ```

2. Build the project:
   ```bash
   cargo build --release
   ```

## 🏃 Usage

1. Start your local Ollama instance (defaulting to `localhost:11434`).
2. Run `ollamaMQ`:
   ```bash
   cargo run
   ```
   The dispatcher will start listening on `http://0.0.0.0:11435`.

### API Proxying

Point your LLM clients to the `ollamaMQ` port (`11435`) and include the `X-User-ID` header.

#### Supported Endpoints:
- `POST /api/generate` (Ollama Native)
- `POST /api/chat` (Ollama Native)
- `POST /v1/chat/completions` (OpenAI Compatible)
- `POST /v1/completions` (OpenAI Compatible)

#### Example (cURL):
```bash
curl -X POST http://localhost:11435/api/chat \
  -H "X-User-ID: developer-1" \
  -d '{
    "model": "llama3",
    "messages": [{"role": "user", "content": "Explain quantum computing."}],
    "stream": true
  }'
```

### Dashboard Controls

The interactive TUI dashboard provides a live view of the dispatcher's state:

- **`j` / `k`** or **Arrows**: Navigate the user list.
- **`q`** or **Esc**: Exit the dashboard.
- **`h`**: Toggle detailed help.

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

- **Run Tests**: `cargo test`
- **Linting**: `cargo clippy`
- **Formatting**: `cargo fmt`

### Stress Testing

You can use the provided `test_dispatcher.sh` script to simulate multiple users and verify the dispatcher's behavior under load:

```bash
./test_dispatcher.sh
```

![ollamaMQ Stress Test](demo-test.png)

## 📝 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details (if applicable).

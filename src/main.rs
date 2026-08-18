use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::{any, get, post},
};
use clap::Parser;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod control;
mod dispatcher;
mod tui;

use crate::control::{admin_model_load, admin_model_unload, admin_models_state};
use crate::dispatcher::{AppState, proxy_handler, run_worker};

use std::io::IsTerminal;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value_t = 11435)]
    port: u16,

    /// Host/interface to bind to
    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    host: String,

    /// Backend server URLs (e.g., Ollama, LM Studio) (comma-separated list)
    #[arg(
        short,
        long,
        value_delimiter = ',',
        default_value = "http://localhost:11434",
        alias = "ollama-urls"
    )]
    backend_urls: Vec<String>,

    /// Request timeout in seconds
    #[arg(short, long, default_value_t = 300)]
    timeout: u64,

    /// Disable TUI dashboard
    #[arg(long)]
    no_tui: bool,

    /// Allow all routes (enable fallback proxy)
    #[arg(long, default_value_t = false)]
    allow_all_routes: bool,

    /// How long (seconds) models stay loaded after a model-control "load"
    /// request (sent as Ollama `keep_alive`; LM Studio loads are unaffected
    /// beyond its own TTL). Ollama's own default is only 5 minutes.
    #[arg(long, default_value_t = 86400)]
    load_keep_alive: u64,
}

struct TuiState {
    visible: bool,
    toggle_notify: Arc<Notify>,
}

/// Constant-time string comparison to avoid timing side-channels when checking API keys.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Build the 401 response with JSON content type and a Bearer challenge hint.
fn unauthorized_response() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("content-type", "application/json")
        .header("www-authenticate", "Bearer")
        .body(axum::body::Body::from("{\"error\":\"unauthorized\"}"))
        .unwrap()
}

/// Optional API-key auth middleware. If no key is configured (state is None),
/// all requests pass through untouched.
async fn auth_middleware(
    State(api_key): State<Option<String>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = api_key else {
        return next.run(request).await;
    };

    let headers = request.headers();
    let provided = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                // Auth-scheme is case-insensitive per RFC 7235; "Bearer " is 7 ASCII bytes.
                .and_then(|v| {
                    if v.len() > 7 && v.get(..7)?.eq_ignore_ascii_case("Bearer ") {
                        Some(v[7..].to_string())
                    } else {
                        None
                    }
                })
        });

    let provided = match provided {
        Some(p) => p,
        None => return unauthorized_response(),
    };

    if constant_time_eq(&provided, &expected) {
        next.run(request).await
    } else {
        unauthorized_response()
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let backend_urls: Vec<String> = args
        .backend_urls
        .iter()
        .map(|url| {
            let trimmed = url.trim_end_matches('/').to_string();
            if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
                format!("http://{}", trimmed)
            } else {
                trimmed
            }
        })
        .collect();

    // Determine if we should run TUI
    let use_tui = !args.no_tui && std::io::stdout().is_terminal();

    // Keep the guard alive for the duration of main
    let _guard: Option<tracing_appender::non_blocking::WorkerGuard>;

    if use_tui {
        let file_appender = tracing_appender::rolling::never(".", "ollamamq.log");
        let (non_blocking, g) = tracing_appender::non_blocking(file_appender);
        _guard = Some(g);

        tracing_subscriber::fmt()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .init();
    } else {
        _guard = None;
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug")),
            )
            .init();
    }

    let state = Arc::new(AppState::new(
        backend_urls,
        args.timeout,
        args.load_keep_alive,
    ));

    let worker_state = state.clone();
    tokio::spawn(async move {
        run_worker(worker_state).await;
    });

    // Optional API-key auth: enabled only when OLLAMA_MQ_API_KEY is set (non-empty).
    let api_key = std::env::var("OLLAMA_MQ_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());

    let mut app = Router::new()
        // Ollama API Endpoints (Explicitly listed)
        .route("/", any(proxy_handler))
        .route("/api/generate", any(proxy_handler))
        .route("/api/chat", any(proxy_handler))
        .route("/api/embed", any(proxy_handler))
        .route("/api/embeddings", any(proxy_handler))
        .route("/api/tags", any(proxy_handler))
        .route("/api/show", any(proxy_handler))
        .route("/api/create", any(proxy_handler))
        .route("/api/copy", any(proxy_handler))
        .route("/api/delete", any(proxy_handler))
        .route("/api/pull", any(proxy_handler))
        .route("/api/push", any(proxy_handler))
        .route("/api/blobs/{digest}", any(proxy_handler))
        .route("/api/ps", any(proxy_handler))
        .route("/api/version", any(proxy_handler))
        // OpenAI Compatible Endpoints
        .route("/v1/chat/completions", any(proxy_handler))
        .route("/v1/completions", any(proxy_handler))
        .route("/v1/embeddings", any(proxy_handler))
        .route("/v1/models", any(proxy_handler))
        .route("/v1/models/{model}", any(proxy_handler))
        // Local admin API (model load/unload control) — never proxied to
        // backends, protected by the same optional API-key auth.
        .route("/admin/models", get(admin_models_state))
        .route("/admin/models/load", post(admin_model_load))
        .route("/admin/models/unload", post(admin_model_unload));

    // Optional fallback
    if args.allow_all_routes {
        app = app.fallback(proxy_handler);
    }

    // Protect all proxy routes (including the fallback) with optional API-key auth.
    // /health is registered separately below and stays unauthenticated.
    let app = app
        .layer(middleware::from_fn_with_state(api_key, auth_middleware))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024 * 1024)) // 1GB limit
        .with_state(state.clone());

    let app = Router::new()
        .route("/health", get(|| async { "OK" }))
        .merge(app)
        .with_state(state.clone());

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    info!("Dispatcher running on http://{}", addr);

    if use_tui {
        let tui_state = Arc::new(Mutex::new(TuiState {
            visible: true,
            toggle_notify: Arc::new(Notify::new()),
        }));

        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        // Run TUI on the main thread
        tui_loop(tui_state, state).await;
    } else {
        // Just run the server on the main thread
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    }
}

async fn tui_loop(tui_state: Arc<Mutex<TuiState>>, state: Arc<AppState>) {
    let mut dashboard = tui::TuiDashboard::new();
    let toggle_notify = Arc::new(tui_state.lock().unwrap().toggle_notify.clone());

    loop {
        let visible = {
            let tui_state = tui_state.lock().unwrap();
            tui_state.visible
        };

        if visible {
            match dashboard.run(&state) {
                Ok(continue_loop) => {
                    if !continue_loop {
                        return;
                    }
                }
                Err(e) => {
                    eprintln!("TUI error: {}", e);
                    return;
                }
            }
        } else {
            toggle_notify.notified().await;
        }
    }
}

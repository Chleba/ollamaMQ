use axum::{
    Router,
    routing::{get, post},
};
use clap::Parser;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod tui;
mod dispatcher;

use crate::dispatcher::{AppState, run_worker, proxy_handler};

use std::io::IsTerminal;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value_t = 11435)]
    port: u16,

    /// Ollama server URL
    #[arg(short, long, default_value = "http://localhost:11434")]
    ollama_url: String,

    /// Disable TUI dashboard
    #[arg(long)]
    no_tui: bool,
}

struct TuiState {
    visible: bool,
    toggle_notify: Arc<Notify>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let ollama_url = args.ollama_url.trim_end_matches('/').to_string();
    
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
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .init();
    }

    let state = Arc::new(AppState::new(ollama_url));

    let worker_state = state.clone();
    tokio::spawn(async move {
        run_worker(worker_state).await;
    });

    let app = Router::new()
        .route("/health", get(|| async { "OK" }))
        .route("/api/generate", post(proxy_handler))
        .route("/api/chat", post(proxy_handler))
        .route("/v1/chat/completions", post(proxy_handler))
        .route("/v1/completions", post(proxy_handler))
        .layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024))
        .with_state(state.clone());

    let addr = format!("0.0.0.0:{}", args.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap();
    info!("Dispatcher running on http://{}", addr);

    if use_tui {
        let tui_state = Arc::new(Mutex::new(TuiState {
            visible: true,
            toggle_notify: Arc::new(Notify::new()),
        }));

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Run TUI on the main thread
        tui_loop(tui_state, state).await;
    } else {
        // Just run the server on the main thread
        axum::serve(listener, app).await.unwrap();
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
                        let mut tui_state = tui_state.lock().unwrap();
                        tui_state.visible = false;
                        tui_state.toggle_notify.notify_one();
                    }
                }
                Err(e) => {
                    eprintln!("TUI error: {}", e);
                    let mut tui_state = tui_state.lock().unwrap();
                    tui_state.visible = false;
                    tui_state.toggle_notify.notify_one();
                }
            }
        } else {
            toggle_notify.notified().await;
        }
    }
}

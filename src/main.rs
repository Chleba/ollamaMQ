use axum::{
    Router,
    routing::post,
};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod tui;
mod dispatcher;

use crate::dispatcher::{AppState, run_worker, proxy_handler};

struct TuiState {
    visible: bool,
    toggle_notify: Arc<Notify>,
}

#[tokio::main]
async fn main() {
    let file_appender = tracing_appender::rolling::never(".", "ollamamq.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let state = Arc::new(AppState::new());

    let tui_state = Arc::new(Mutex::new(TuiState {
        visible: true,
        toggle_notify: Arc::new(Notify::new()),
    }));

    let worker_state = state.clone();
    tokio::spawn(async move {
        run_worker(worker_state).await;
    });

    let app = Router::new()
        .route("/api/generate", post(proxy_handler))
        .route("/api/chat", post(proxy_handler))
        .route("/v1/chat/completions", post(proxy_handler))
        .route("/v1/completions", post(proxy_handler))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:11435")
        .await
        .unwrap();
    info!("Dispatcher running on http://0.0.0.0:11435");

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Run TUI on the main thread
    tui_loop(tui_state, state).await;
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

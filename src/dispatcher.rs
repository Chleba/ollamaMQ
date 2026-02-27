use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap},
    response::IntoResponse,
};
use futures_util::StreamExt;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, Notify};
use tokio_stream::wrappers::ReceiverStream;
use tracing::info;

pub struct Task {
    pub path: String,
    pub body: Bytes,
    pub responder: mpsc::Sender<Result<Bytes, reqwest::Error>>,
}

pub struct AppState {
    pub queues: Mutex<HashMap<String, VecDeque<Task>>>,
    pub processed_counts: Mutex<HashMap<String, usize>>,
    pub dropped_counts: Mutex<HashMap<String, usize>>,
    pub notify: Notify,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            queues: Mutex::new(HashMap::new()),
            processed_counts: Mutex::new(HashMap::new()),
            dropped_counts: Mutex::new(HashMap::new()),
            notify: Notify::new(),
        }
    }
}

pub async fn run_worker(state: Arc<AppState>) {
    // 5-minute timeout for backend requests
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .unwrap();
    let mut current_user_keys: Vec<String> = Vec::new();
    let mut current_idx = 0;

    loop {
        let task_opt = {
            let mut queues = state.queues.lock().unwrap();

            // Sync current_user_keys with actual queues to handle new users
            for user_id in queues.keys() {
                if !current_user_keys.contains(user_id) {
                    current_user_keys.push(user_id.clone());
                }
            }

            // Keep users in keys but only process those with tasks
            let active_users: Vec<String> = current_user_keys
                .iter()
                .filter(|k| queues.contains_key(*k) && !queues.get(*k).unwrap().is_empty())
                .cloned()
                .collect();

            if active_users.is_empty() {
                None
            } else {
                if current_idx >= active_users.len() {
                    current_idx = 0;
                }

                let user_id = active_users[current_idx].clone();
                let task = queues.get_mut(&user_id).unwrap().pop_front().unwrap();
                
                current_idx += 1;
                Some((user_id, task))
            }
        };

        match task_opt {
            Some((user_id, task)) => {
                // Check if client is still connected before processing
                if task.responder.is_closed() {
                    info!("Skipping task for user {} - client disconnected", user_id);
                    let mut dropped = state.dropped_counts.lock().unwrap();
                    *dropped.entry(user_id).or_insert(0) += 1;
                    continue;
                }

                info!("Processing {} for user: {}", task.path, user_id);
                // Artificial delay to make TUI observation easier
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                let url = format!("http://localhost:11434{}", task.path);
                
                let res_fut = client
                    .post(url)
                    .body(task.body)
                    .send();

                tokio::select! {
                    res = res_fut => {
                        match res {
                            Ok(response) => {
                                let mut stream = response.bytes_stream();
                                let mut client_disconnected = false;
                                while let Some(chunk) = stream.next().await {
                                    if task.responder.send(chunk).await.is_err() {
                                        info!("Client disconnected during streaming for user {}", user_id);
                                        client_disconnected = true;
                                        break;
                                    }
                                }
                                
                                if client_disconnected {
                                    let mut dropped = state.dropped_counts.lock().unwrap();
                                    *dropped.entry(user_id).or_insert(0) += 1;
                                } else {
                                    info!("Request {} for user {} completed", task.path, user_id);
                                    let mut counts = state.processed_counts.lock().unwrap();
                                    *counts.entry(user_id).or_insert(0) += 1;
                                }
                            }
                            Err(e) => {
                                info!("Request {} for user {} failed: {}", task.path, user_id, e);
                                let _ = task.responder.send(Err(e)).await;
                                let mut dropped = state.dropped_counts.lock().unwrap();
                                *dropped.entry(user_id).or_insert(0) += 1;
                            }
                        }
                    }
                    _ = task.responder.closed() => {
                        info!("Client disconnected while waiting for backend response for user {}", user_id);
                        let mut dropped = state.dropped_counts.lock().unwrap();
                        *dropped.entry(user_id).or_insert(0) += 1;
                    }
                }
            }
            None => {
                state.notify.notified().await;
            }
        }
    }
}

pub async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    body: Bytes,
) -> impl IntoResponse {
    let path = uri.path().to_string();
    let user_id = headers
        .get("X-User-ID")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("anonymous")
        .to_string();

    info!("Received {} request from user: {}", path, user_id);

    let (tx, rx) = mpsc::channel(32);
    let task = Task {
        path,
        responder: tx,
        body,
    };

    {
        let mut queues = state.queues.lock().unwrap();
        queues
            .entry(user_id.clone())
            .or_insert_with(VecDeque::new)
            .push_back(task);
    }

    state.notify.notify_one();

    let stream = ReceiverStream::new(rx);
    Body::from_stream(stream).into_response()
}

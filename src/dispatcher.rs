use axum::{
    body::{Body, Bytes},
    extract::{State, ConnectInfo},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use futures_util::StreamExt;
use std::{
    collections::{HashMap, VecDeque, HashSet},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    fs,
};
use tokio::sync::{mpsc, Notify};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, warn};
use serde::{Serialize, Deserialize};

const BLOCKED_FILE: &str = "blocked_items.json";

#[derive(Serialize, Deserialize, Default)]
struct BlockedConfig {
    ips: HashSet<IpAddr>,
    users: HashSet<String>,
}

pub struct Task {
    pub path: String,
    pub body: Bytes,
    pub responder: mpsc::Sender<Result<Bytes, reqwest::Error>>,
}

pub struct AppState {
    pub queues: Mutex<HashMap<String, VecDeque<Task>>>,
    pub processed_counts: Mutex<HashMap<String, usize>>,
    pub dropped_counts: Mutex<HashMap<String, usize>>,
    pub user_ips: Mutex<HashMap<String, IpAddr>>,
    pub blocked_ips: Mutex<HashSet<IpAddr>>,
    pub blocked_users: Mutex<HashSet<String>>,
    pub notify: Notify,
    pub ollama_url: String,
}

impl AppState {
    pub fn new(ollama_url: String) -> Self {
        let (blocked_ips, blocked_users) = Self::load_blocked_items();
        Self {
            queues: Mutex::new(HashMap::new()),
            processed_counts: Mutex::new(HashMap::new()),
            dropped_counts: Mutex::new(HashMap::new()),
            user_ips: Mutex::new(HashMap::new()),
            blocked_ips: Mutex::new(blocked_ips),
            blocked_users: Mutex::new(blocked_users),
            notify: Notify::new(),
            ollama_url,
        }
    }

    fn load_blocked_items() -> (HashSet<IpAddr>, HashSet<String>) {
        if let Ok(content) = fs::read_to_string(BLOCKED_FILE) {
            if let Ok(config) = serde_json::from_str::<BlockedConfig>(&content) {
                return (config.ips, config.users);
            }
        }
        (HashSet::new(), HashSet::new())
    }

    fn save_blocked_items(&self) {
        let config = BlockedConfig {
            ips: self.blocked_ips.lock().unwrap().clone(),
            users: self.blocked_users.lock().unwrap().clone(),
        };
        if let Ok(content) = serde_json::to_string_pretty(&config) {
            let _ = fs::write(BLOCKED_FILE, content);
        }
    }

    pub fn block_ip(&self, ip: IpAddr) {
        {
            let mut ips = self.blocked_ips.lock().unwrap();
            ips.insert(ip);
        }
        self.save_blocked_items();
        warn!("IP blocked: {}", ip);
    }

    pub fn block_user(&self, user_id: String) {
        {
            let mut users = self.blocked_users.lock().unwrap();
            users.insert(user_id.clone());
        }
        self.save_blocked_items();
        warn!("User blocked: {}", user_id);
    }

    #[allow(dead_code)]
    pub fn unblock_ip(&self, ip: IpAddr) {
        {
            let mut ips = self.blocked_ips.lock().unwrap();
            ips.remove(&ip);
        }
        self.save_blocked_items();
        info!("IP unblocked: {}", ip);
    }

    #[allow(dead_code)]
    pub fn unblock_user(&self, user_id: &str) {
        {
            let mut users = self.blocked_users.lock().unwrap();
            users.remove(user_id);
        }
        self.save_blocked_items();
        info!("User unblocked: {}", user_id);
    }

    pub fn is_ip_blocked(&self, ip: &IpAddr) -> bool {
        self.blocked_ips.lock().unwrap().contains(ip)
    }

    pub fn is_user_blocked(&self, user_id: &str) -> bool {
        self.blocked_users.lock().unwrap().contains(user_id)
    }
}

pub async fn run_worker(state: Arc<AppState>) {
    // 5-minute timeout for backend requests
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .unwrap();
    let mut current_idx = 0;

    loop {
        let task_opt = {
            let mut queues = state.queues.lock().unwrap();
            
            // Get all user IDs that currently have tasks
            let mut active_users: Vec<String> = queues.iter()
                .filter(|(_, q)| !q.is_empty())
                .map(|(k, _)| k.clone())
                .collect();
            
            // Sort to ensure stable round-robin
            active_users.sort();

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

                let url = format!("{}{}", state.ollama_url, task.path);
                
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
                                let mut first_chunk = true;

                                while let Some(chunk_res) = stream.next().await {
                                    let chunk = match chunk_res {
                                        Ok(c) => c,
                                        Err(e) => {
                                            info!("Error reading from backend: {}", e);
                                            break;
                                        }
                                    };

                                    if first_chunk {
                                        let content = String::from_utf8_lossy(&chunk);
                                        info!("Response for user {}: {}", user_id, content.trim());
                                        first_chunk = false;
                                    }

                                    if task.responder.send(Ok(chunk)).await.is_err() {
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
                info!("Worker idle, waiting for tasks...");
                state.notify.notified().await;
            }
        }
    }
}

pub async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    body: Bytes,
) -> impl IntoResponse {
    let path = uri.path().to_string();
    let ip = addr.ip();
    let user_id = headers
        .get("X-User-ID")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("anonymous")
        .to_string();

    info!("Received {} request from user: {} (IP: {})", path, user_id, ip);

    if state.is_ip_blocked(&ip) {
        warn!("Blocked request from IP: {} for user: {}", ip, user_id);
        return (StatusCode::FORBIDDEN, "IP blocked").into_response();
    }

    if state.is_user_blocked(&user_id) {
        warn!("Blocked request from user: {} (IP: {})", user_id, ip);
        return (StatusCode::FORBIDDEN, "User blocked").into_response();
    }

    {
        let mut ips = state.user_ips.lock().unwrap();
        ips.insert(user_id.clone(), ip);
    }

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
